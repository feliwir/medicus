//! Rendered resources (PS3.18 §10.4.1.1.3–4, §8.3.5.1): an instance or frame decoded and windowed
//! by dicom-pixeldata, then encoded as JPEG, PNG or GIF.
//!
//! A study or series renders its first instance with pixel data, an instance its first frame.
//! Thumbnails are served as rendered resources. Annotations are not burned in (answered with a
//! `Warning`), and ICC profiles are not embedded.

use std::io::Cursor;

use axum::extract::{Query as Params, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use dicom_dictionary_std::tags;
use dicom_encoding::adapters::PixelDataObject as _;
use dicom_pixeldata::image::{DynamicImage, ImageFormat, imageops};
use dicom_pixeldata::{
    ConvertOptions, PixelDecoder as _, VoiLutFunction, VoiLutOption, WindowLevel,
};
use image::codecs::jpeg::JpegEncoder;

use crate::error::{WebError, WebResult};
use crate::wado::{self, FrameError};
use crate::web::{Request, State as Web};

/// JPEG quality when the request does not give one.
const DEFAULT_QUALITY: u8 = 90;

/// The rendered media types offered (PS3.18 Table 8.7.4-1, single-frame images).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Media {
    Jpeg,
    Png,
    Gif,
}

impl Media {
    fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
        }
    }

    fn parse(media: &str) -> Option<Self> {
        match media {
            "image/jpeg" | "image/*" | "*/*" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }
}

/// `viewport=vw,vh[,sx,sy,sw,sh]` (PS3.18 §8.3.5.1.3).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Viewport {
    width: u32,
    height: u32,
    sx: Option<f64>,
    sy: Option<f64>,
    /// Negative: flipped horizontally.
    sw: Option<f64>,
    /// Negative: flipped vertically.
    sh: Option<f64>,
}

/// How to render: the query parameters of PS3.18 Table 8.3.5-1 that are supported.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Options {
    media: Media,
    quality: u8,
    viewport: Option<Viewport>,
    window: Option<(WindowLevel, VoiLutFunction)>,
}

impl Options {
    /// The options of a request's query and Accept header; the media type from the `accept`
    /// parameter if given, else from the header, else JPEG.
    fn parse(params: &[(String, String)], headers: &HeaderMap) -> WebResult<Self> {
        let param = |name: &str| {
            params
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let accepted: Vec<String> = match param("accept") {
            Some(list) => list
                .split(',')
                .filter_map(|m| m.split(';').next())
                .map(|m| m.trim().to_ascii_lowercase())
                .collect(),
            None => wado::accepted(headers)
                .into_iter()
                .map(|a| a.media)
                .collect(),
        };
        let media = if accepted.is_empty() {
            Media::Jpeg
        } else {
            accepted
                .iter()
                .find_map(|m| Media::parse(m))
                .ok_or_else(|| {
                    WebError::not_acceptable(format!(
                        "no supported rendered media type in {accepted:?}; \
                         offered: image/jpeg, image/png, image/gif"
                    ))
                })?
        };
        let quality = match param("quality") {
            Some(q) => match q.trim().parse::<u8>() {
                Ok(q @ 1..=100) => q,
                _ => return Err(WebError::bad_request(format!("quality {q:?}: not 1..=100"))),
            },
            None => DEFAULT_QUALITY,
        };
        Ok(Self {
            media,
            quality,
            viewport: param("viewport").map(parse_viewport).transpose()?,
            window: param("window").map(parse_window).transpose()?,
        })
    }
}

fn parse_viewport(raw: &str) -> WebResult<Viewport> {
    let bad = || WebError::bad_request(format!("viewport {raw:?}: expected vw,vh[,sx,sy,sw,sh]"));
    let values: Vec<&str> = raw.split(',').map(str::trim).collect();
    if values.len() != 2 && values.len() != 6 {
        return Err(bad());
    }
    let size = |s: &str| s.parse::<u32>().ok().filter(|&n| n > 0).ok_or_else(bad);
    let decimal = |i: usize| -> WebResult<Option<f64>> {
        match values.get(i) {
            None | Some(&"") => Ok(None),
            Some(s) => s
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(Some)
                .ok_or_else(bad),
        }
    };
    Ok(Viewport {
        width: size(values[0])?,
        height: size(values[1])?,
        sx: decimal(2)?,
        sy: decimal(3)?,
        sw: decimal(4)?,
        sh: decimal(5)?,
    })
}

/// `window=center,width,function` (PS3.18 §8.3.5.1.4); the function defaults to linear.
fn parse_window(raw: &str) -> WebResult<(WindowLevel, VoiLutFunction)> {
    let bad = || {
        WebError::bad_request(format!(
            "window {raw:?}: expected center,width,linear|linear-exact|sigmoid"
        ))
    };
    let values: Vec<&str> = raw.split(',').map(str::trim).collect();
    if values.len() != 2 && values.len() != 3 {
        return Err(bad());
    }
    let decimal = |s: &str| {
        s.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(bad)
    };
    let function = match values.get(2).map(|f| f.to_ascii_lowercase()).as_deref() {
        None | Some("linear") => VoiLutFunction::Linear,
        Some("linear-exact") => VoiLutFunction::LinearExact,
        Some("sigmoid") => VoiLutFunction::Sigmoid,
        Some(_) => return Err(bad()),
    };
    let level = WindowLevel {
        center: decimal(values[0])?,
        width: decimal(values[1])?,
    };
    Ok((level, function))
}

/// A viewport's source coordinate in whole pixels.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pixels(v: f64) -> u32 {
    v.abs().round().min(f64::from(u32::MAX)) as u32
}

/// Crops to the source region, flips for negative extents, then scales to fit the viewport
/// keeping the aspect ratio, centred on a black canvas of the viewport's size.
fn apply_viewport(image: &DynamicImage, viewport: &Viewport) -> Result<DynamicImage, FrameError> {
    let x = viewport.sx.map_or(0, pixels);
    let y = viewport.sy.map_or(0, pixels);
    let width = viewport
        .sw
        .map_or_else(|| image.width().saturating_sub(x), pixels);
    let height = viewport
        .sh
        .map_or_else(|| image.height().saturating_sub(y), pixels);
    let mut region = image.crop_imm(x, y, width, height);
    if region.width() == 0 || region.height() == 0 {
        return Err(FrameError::Unreadable(
            "the viewport's source region lies outside the image".to_owned(),
        ));
    }
    if viewport.sw.is_some_and(|w| w < 0.0) {
        region = region.fliph();
    }
    if viewport.sh.is_some_and(|h| h < 0.0) {
        region = region.flipv();
    }
    let scaled = region.resize(
        viewport.width,
        viewport.height,
        imageops::FilterType::Triangle,
    );
    let mut canvas = DynamicImage::new(viewport.width, viewport.height, scaled.color());
    let dx = (viewport.width - scaled.width()) / 2;
    let dy = (viewport.height - scaled.height()) / 2;
    imageops::overlay(&mut canvas, &scaled, i64::from(dx), i64::from(dy));
    Ok(canvas)
}

/// Frame `number` (1-based) of a stored Part 10 file, rendered as `options` ask.
pub(crate) fn render(file: &[u8], number: usize, options: &Options) -> Result<Vec<u8>, FrameError> {
    let obj = dicom_object::OpenFileOptions::new()
        .from_reader(file)
        .map_err(|e| FrameError::Unreadable(e.to_string()))?;
    if obj.get(tags::PIXEL_DATA).is_none() {
        return Err(FrameError::NoPixelData);
    }
    let frames = obj.number_of_frames().unwrap_or(1) as usize;
    if number > frames {
        return Err(FrameError::NoSuchFrame(number));
    }
    let index = u32::try_from(number - 1).map_err(|_| FrameError::NoSuchFrame(number))?;
    let unreadable = |e: dicom_pixeldata::Error| FrameError::Unreadable(e.to_string());
    let decoded = obj.decode_pixel_data_frame(index).map_err(unreadable)?;
    let mut convert = ConvertOptions::new().force_8bit();
    if let Some((level, function)) = options.window {
        convert = convert.with_voi_lut(VoiLutOption::CustomWithFunction(level, function));
    }
    let mut image = decoded
        .to_dynamic_image_with_options(0, &convert)
        .map_err(unreadable)?;
    if let Some(viewport) = &options.viewport {
        image = apply_viewport(&image, viewport)?;
    }
    let mut out = Vec::new();
    let encoded = match options.media {
        Media::Jpeg => {
            image.write_with_encoder(JpegEncoder::new_with_quality(&mut out, options.quality))
        }
        Media::Png => image.write_to(Cursor::new(&mut out), ImageFormat::Png),
        // The GIF encoder takes RGBA only.
        Media::Gif => {
            DynamicImage::from(image.to_rgba8()).write_to(Cursor::new(&mut out), ImageFormat::Gif)
        }
    };
    encoded.map_err(|e| FrameError::Unreadable(e.to_string()))?;
    Ok(out)
}

/// `GET .../rendered` and `.../thumbnail` of a study, series, instance or frame.
pub(crate) async fn rendered(
    State(web): State<Web>,
    req: Request,
    headers: HeaderMap,
    Params(params): Params<Vec<(String, String)>>,
) -> WebResult<Response> {
    let options = Options::parse(&params, &headers)?;
    let number = match req.params.get("frames") {
        Some(list) => match wado::frame_numbers(list)?.as_slice() {
            [n] => *n,
            _ => {
                return Err(WebError::bad_request(
                    "a rendered single-frame media type holds exactly one frame",
                ));
            }
        },
        None => 1,
    };
    let found = wado::locate(&web, &req).await?;
    let mut rendered = None;
    for location in &found {
        let file = wado::read_instance(&web, &req, location).await?;
        let options = options.clone();
        let result = tokio::task::spawn_blocking(move || render(&file, number, &options))
            .await
            .map_err(|e| WebError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        match result {
            // A study or series renders its first image; skip e.g. presentation states.
            Err(FrameError::NoPixelData) => {}
            other => {
                rendered = Some(other?);
                break;
            }
        }
    }
    let body = rendered.ok_or(FrameError::NoPixelData)?;
    let mut response = ([(header::CONTENT_TYPE, options.media.as_str())], body).into_response();
    let annotations: Vec<&str> = params
        .iter()
        .filter(|(k, _)| k == "annotation")
        .map(|(_, v)| v.as_str())
        .collect();
    if !annotations.is_empty() {
        let warning = format!(
            "299 medicus: The following annotation values are not supported: {}",
            annotations.join(",")
        );
        if let Ok(value) = HeaderValue::from_str(&warning) {
            response.headers_mut().insert(header::WARNING, value);
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(query: &str, accept: Option<&str>) -> WebResult<Options> {
        let params: Vec<(String, String)> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            headers.insert(header::ACCEPT, accept.parse().unwrap());
        }
        Options::parse(&params, &headers)
    }

    #[test]
    fn media_type_negotiation() {
        assert_eq!(options("", None).unwrap().media, Media::Jpeg);
        assert_eq!(options("", Some("image/png")).unwrap().media, Media::Png);
        // A browser's <img> request.
        let browser = "image/avif,image/webp,image/apng,image/*,*/*;q=0.8";
        assert_eq!(options("", Some(browser)).unwrap().media, Media::Jpeg);
        // The query parameter wins over the header.
        assert_eq!(
            options("accept=image/gif", Some("image/png"))
                .unwrap()
                .media,
            Media::Gif
        );
        assert_eq!(
            options("", Some("video/mp4")).unwrap_err().status(),
            StatusCode::NOT_ACCEPTABLE
        );
    }

    #[test]
    fn query_parameters() {
        let o = options(
            "quality=50&viewport=64,32,,,10,-20&window=40,400,sigmoid",
            None,
        )
        .unwrap();
        assert_eq!(o.quality, 50);
        assert_eq!(
            o.viewport,
            Some(Viewport {
                width: 64,
                height: 32,
                sx: None,
                sy: None,
                sw: Some(10.0),
                sh: Some(-20.0),
            })
        );
        let (level, function) = o.window.unwrap();
        assert_eq!((level.center, level.width), (40.0, 400.0));
        assert_eq!(function, VoiLutFunction::Sigmoid);
        assert!(options("quality=0", None).is_err());
        assert!(options("viewport=0,10", None).is_err());
        assert!(options("viewport=1,2,3", None).is_err());
        assert!(options("window=1,2,cubic", None).is_err());
    }

    /// A 4×2 MONOCHROME2 image of `frames` frames, frame `f` filled with `f * 100`.
    fn file(frames: u8) -> Vec<u8> {
        use dicom_core::{DataElement, PrimitiveValue, VR};
        let us = |tag, v: u16| DataElement::new(tag, VR::US, PrimitiveValue::from(v));
        let pixels: Vec<u8> = (0..frames).flat_map(|f| [f * 100; 8]).collect();
        let obj = dicom_object::InMemDicomObject::from_element_iter([
            DataElement::new(
                tags::SOP_CLASS_UID,
                VR::UI,
                PrimitiveValue::from("1.2.840.10008.5.1.4.1.1.7"),
            ),
            DataElement::new(
                tags::SOP_INSTANCE_UID,
                VR::UI,
                PrimitiveValue::from("1.2.3"),
            ),
            us(tags::ROWS, 2),
            us(tags::COLUMNS, 4),
            us(tags::BITS_ALLOCATED, 8),
            us(tags::BITS_STORED, 8),
            us(tags::HIGH_BIT, 7),
            us(tags::PIXEL_REPRESENTATION, 0),
            us(tags::SAMPLES_PER_PIXEL, 1),
            DataElement::new(
                tags::NUMBER_OF_FRAMES,
                VR::IS,
                PrimitiveValue::from(frames.to_string()),
            ),
            DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("MONOCHROME2"),
            ),
            DataElement::new(tags::PIXEL_DATA, VR::OB, PrimitiveValue::from(pixels)),
        ])
        .with_meta(
            dicom_object::meta::FileMetaTableBuilder::new().transfer_syntax("1.2.840.10008.1.2.1"),
        )
        .unwrap();
        let mut out = Vec::new();
        obj.write_all(&mut out).unwrap();
        out
    }

    fn decode(bytes: &[u8]) -> DynamicImage {
        image::load_from_memory(bytes).unwrap()
    }

    #[test]
    fn renders_frames_in_every_media_type() {
        let f = file(3);
        for (media, format) in [
            (Media::Jpeg, ImageFormat::Jpeg),
            (Media::Png, ImageFormat::Png),
            (Media::Gif, ImageFormat::Gif),
        ] {
            let o = Options {
                media,
                ..options("", None).unwrap()
            };
            let out = render(&f, 2, &o).unwrap();
            assert_eq!(image::guess_format(&out).unwrap(), format);
            let img = decode(&out);
            assert_eq!((img.width(), img.height()), (4, 2));
        }
        assert_eq!(
            render(&f, 4, &options("", None).unwrap()),
            Err(FrameError::NoSuchFrame(4))
        );
    }

    #[test]
    fn window_and_viewport() {
        let f = file(3);
        // Frame 2 holds 100: below a window of 200±50 it is black, inside 0..200 it is grey.
        let png =
            |q: &str| decode(&render(&f, 2, &options(q, Some("image/png")).unwrap()).unwrap());
        assert_eq!(
            png("window=200,100,linear").to_luma8().get_pixel(0, 0).0,
            [0]
        );
        let grey = png("window=100,200,linear").to_luma8().get_pixel(0, 0).0[0];
        assert!((100..160).contains(&grey), "{grey}");
        // 4×2 scaled into 16×16: 16×8 centred vertically.
        let scaled = png("viewport=16,16&window=100,200,linear").to_luma8();
        assert_eq!(scaled.dimensions(), (16, 16));
        assert_eq!(scaled.get_pixel(8, 0).0, [0]);
        assert_ne!(scaled.get_pixel(8, 8).0, [0]);
        assert!(
            render(&f, 1, &options("viewport=4,4,10,10,,", None).unwrap()).is_err(),
            "source region outside the image"
        );
    }
}

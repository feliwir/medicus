//! Deterministic DICOM Part 10 test files, written by dicom-rs.

use dicom_core::header::Header as _;
use dicom_core::value::DataSetSequence;
use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::TransferSyntaxIndex as _;
use dicom_object::InMemDicomObject;
use dicom_object::meta::FileMetaTableBuilder;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;

/// Builds a CT image instance with configurable attributes and pixel data.
#[derive(Debug, Clone)]
pub struct InstanceBuilder {
    transfer_syntax: String,
    elements: Vec<DataElement<InMemDicomObject>>,
    pixel_data: usize,
    sequence: bool,
}

impl InstanceBuilder {
    pub fn new(study: &str, series: &str, sop: &str) -> Self {
        let el = |tag: Tag, vr: VR, v: &str| DataElement::new(tag, vr, PrimitiveValue::from(v));
        Self {
            transfer_syntax: uids::EXPLICIT_VR_LITTLE_ENDIAN.to_owned(),
            elements: vec![
                el(tags::SOP_CLASS_UID, VR::UI, uids::CT_IMAGE_STORAGE),
                el(tags::SOP_INSTANCE_UID, VR::UI, sop),
                el(tags::STUDY_INSTANCE_UID, VR::UI, study),
                el(tags::SERIES_INSTANCE_UID, VR::UI, series),
                el(tags::PATIENT_ID, VR::LO, "PID-1"),
                el(tags::PATIENT_NAME, VR::PN, "Doe^John"),
                el(tags::MODALITY, VR::CS, "CT"),
                el(tags::STUDY_DATE, VR::DA, "20240115"),
                el(tags::INSTANCE_NUMBER, VR::IS, "1"),
            ],
            pixel_data: 512,
            sequence: false,
        }
    }

    #[must_use]
    pub fn transfer_syntax(mut self, ts: &str) -> Self {
        ts.clone_into(&mut self.transfer_syntax);
        self
    }

    /// Sets (or replaces) a string-valued attribute; `\` separates multiple values.
    #[must_use]
    pub fn attr(mut self, tag: Tag, vr: VR, value: &str) -> Self {
        self.elements.retain(|e| e.tag() != tag);
        let v = if value.contains('\\') {
            PrimitiveValue::Strs(value.split('\\').map(str::to_owned).collect())
        } else {
            PrimitiveValue::from(value)
        };
        self.elements.push(DataElement::new(tag, vr, v));
        self
    }

    /// Size of the (dummy) Pixel Data in bytes; 0 omits Pixel Data.
    #[must_use]
    pub fn pixel_data(mut self, bytes: usize) -> Self {
        self.pixel_data = bytes;
        self
    }

    /// Makes it a renderable 8-bit MONOCHROME2 image of `rows` × `columns`, with matching Pixel
    /// Data.
    #[must_use]
    pub fn image(mut self, rows: u16, columns: u16) -> Self {
        let us = |tag: Tag, v: u16| DataElement::new(tag, VR::US, PrimitiveValue::from(v));
        let attrs = [
            us(tags::ROWS, rows),
            us(tags::COLUMNS, columns),
            us(tags::BITS_ALLOCATED, 8),
            us(tags::BITS_STORED, 8),
            us(tags::HIGH_BIT, 7),
            us(tags::PIXEL_REPRESENTATION, 0),
            us(tags::SAMPLES_PER_PIXEL, 1),
            DataElement::new(
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                PrimitiveValue::from("MONOCHROME2"),
            ),
        ];
        self.elements
            .retain(|e| !attrs.iter().any(|a| a.tag() == e.tag()));
        self.elements.extend(attrs);
        self.pixel_data = usize::from(rows) * usize::from(columns);
        self
    }

    /// Adds a Procedure Code Sequence with two items.
    #[must_use]
    pub fn with_sequence(mut self) -> Self {
        self.sequence = true;
        self
    }

    /// The complete Part 10 file.
    pub fn build(&self) -> Vec<u8> {
        let file = self
            .object()
            .with_meta(FileMetaTableBuilder::new().transfer_syntax(self.transfer_syntax.as_str()))
            .expect("valid meta");
        let mut out = Vec::new();
        file.write_all(&mut out).expect("in-memory write");
        out
    }

    /// Only the data set, encoded in the transfer syntax (as a C-STORE carries it).
    pub fn build_data_set(&self) -> Vec<u8> {
        let ts = TransferSyntaxRegistry
            .get(&self.transfer_syntax)
            .expect("known transfer syntax");
        let mut out = Vec::new();
        self.object()
            .write_dataset_with_ts(&mut out, ts)
            .expect("in-memory write");
        out
    }

    /// The configured SOP Instance UID.
    pub fn sop_instance(&self) -> String {
        self.elements
            .iter()
            .find(|e| e.tag() == tags::SOP_INSTANCE_UID)
            .and_then(|e| e.to_str().ok())
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default()
    }

    fn object(&self) -> InMemDicomObject {
        let mut obj = InMemDicomObject::from_element_iter(self.elements.clone());
        if self.sequence {
            let item = |code: &str| {
                InMemDicomObject::from_element_iter([
                    DataElement::new(tags::CODE_VALUE, VR::SH, PrimitiveValue::from(code)),
                    DataElement::new(
                        tags::CODING_SCHEME_DESIGNATOR,
                        VR::SH,
                        PrimitiveValue::from("L"),
                    ),
                ])
            };
            obj.put(DataElement::new(
                tags::PROCEDURE_CODE_SEQUENCE,
                VR::SQ,
                DataSetSequence::from(vec![item("ABC"), item("XYZ")]),
            ));
        }
        if self.pixel_data > 0 {
            let bytes: Vec<u8> = (0..self.pixel_data)
                .map(|i| u8::try_from(i % 251).unwrap())
                .collect();
            obj.put(DataElement::new(
                tags::PIXEL_DATA,
                VR::OB,
                PrimitiveValue::from(bytes),
            ));
        }
        obj
    }
}

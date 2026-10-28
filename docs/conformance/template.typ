// Layout of the conformance statement PDF: cover, contents, running header and footer, and the
// styling of the Markdown rendered by cmarker. Neutral by design; pass `logo` to brand it.

#let accent = rgb("#1d4f7a")
#let muted = luma(110)
#let rule = luma(205)

#let statement(
  title: "DICOM Conformance Statement",
  product: "medicus",
  version: none,
  date: datetime.today(),
  logo: none,
  body,
) = {
  set document(title: product + " " + title, author: product)
  set text(font: ("Inter", "Noto Sans", "DejaVu Sans"), size: 10pt, lang: "en")
  set par(justify: true, leading: 0.7em, spacing: 1.1em)
  set page(paper: "a4", margin: (x: 2.3cm, top: 2.7cm, bottom: 2.5cm))

  // Cover.
  page(margin: (x: 2.3cm, top: 3.5cm, bottom: 2.5cm))[
    #if logo != none { image(logo, height: 1.6cm); v(1.2cm) }
    #text(size: 12pt, weight: "medium", fill: accent, tracking: 0.08em, upper(product))
    #v(0.4cm)
    #text(size: 30pt, weight: "bold", hyphenate: false, title)
    #v(0.3cm)
    #line(length: 3cm, stroke: 2.5pt + accent)
    #v(1fr)
    #grid(
      columns: (auto, 1fr),
      column-gutter: 1.2em,
      row-gutter: 0.8em,
      text(fill: muted)[Product], [#product],
      text(fill: muted)[Version], [#if version != none { version } else { [development] }],
      text(fill: muted)[Date], date.display("[day] [month repr:long] [year]"),
      text(fill: muted)[Standard], [DICOM PS3.2 (Conformance)],
    )
    #v(1.2cm)
    #text(size: 8pt, fill: muted)[
      This document is generated from `docs/conformance/conformance-statement.md`; the tables
      of SOP classes and transfer syntaxes are produced from the source code of this version.
    ]
  ]

  set page(
    header: context {
      set text(size: 8pt, fill: muted)
      grid(
        columns: (1fr, auto),
        [#product #title],
        [#if version != none [Version #version]],
      )
      v(-0.5em)
      line(length: 100%, stroke: 0.5pt + rule)
    },
    footer: context {
      set text(size: 8pt, fill: muted)
      align(center, counter(page).display("1 / 1", both: true))
    },
  )

  // Headings: numbered sections, each level-1 section on a new page.
  set heading(numbering: "1.1")
  show heading: set text(fill: accent, hyphenate: false)
  show heading: set par(justify: false)
  show heading.where(level: 1): it => {
    pagebreak(weak: true)
    v(0.2cm)
    text(size: 17pt, it)
    v(0.3cm)
  }
  show heading.where(level: 2): set text(size: 12.5pt)
  show heading.where(level: 3): set text(size: 10.5pt)
  show heading: set block(above: 1.6em, below: 0.9em)

  // Tables: a tinted header row, hairlines between rows, compact text.
  set table(
    inset: (x: 6pt, y: 4.5pt),
    stroke: (_, y) => (
      top: if y == 0 { 1pt + accent } else { none },
      bottom: 0.5pt + rule,
    ),
    fill: (_, y) => if y == 0 { accent.lighten(88%) } else if calc.even(y) { luma(248) },
  )
  show table: set text(size: 8.5pt)
  show table: set par(justify: false)
  show table.cell.where(y: 0): set text(weight: "bold")
  show table: it => block(width: 100%, breakable: true, it)

  show raw: set text(font: ("DejaVu Sans Mono",), size: 0.92em)
  show link: set text(fill: accent)
  set list(indent: 0.6em)
  set enum(indent: 0.6em)

  // Contents.
  {
    show outline.entry.where(level: 1): set text(weight: "bold")
    show outline.entry.where(level: 1): set block(above: 1em)
    outline(title: [Contents], depth: 2, indent: auto)
  }

  body
}

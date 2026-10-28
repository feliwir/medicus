// The PDF of the conformance statement, rendered from the Markdown source:
//
//   typst compile docs/conformance/conformance-statement.typ
//
// Needs Typst 0.15 (it downloads the cmarker package on first use). The Markdown file is the
// source of truth and reads on its own; this file only lays it out.

#import "@preview/cmarker:0.1.10"
#import "template.typ": statement

#let source = read("conformance-statement.md")
// The version is the one generated into the product table.
#let version = {
  let found = source.match(regex("\|\s*medicus\s*\|\s*([^\s|]+)\s*\|"))
  if found != none { found.captures.at(0) }
}

#show: statement.with(version: version)

// `#` is the document title (left out of the PDF, which has a cover); `##` are the sections.
#cmarker.render(source, h1-level: 0, set-document-title: false)

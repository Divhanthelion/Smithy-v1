//! Wavy underlines for diagnostics.
//!
//! These cannot come through `Styling`. `apply_attr_styles` returns a
//! cosmic-text `Attrs`, which has no underline and no background field, so the
//! most the colouring layer can do to a diagnostic is recolour and embolden its
//! characters — which it does. A squiggle has to be painted as a separate
//! layer over the text.
//!
//! Everything in this module is geometry, and none of it touches floem's
//! editor. That is deliberate: the hard part of drawing under text is deciding
//! *where*, and under soft wrap one diagnostic becomes several separate runs on
//! several visual rows. That decision is arithmetic and is tested here. The
//! view in [`crate::code_editor`] does only what cannot be tested — asks floem
//! for the x of a byte offset, and strokes.

use std::ops::Range;

use floem::peniko::kurbo::{BezPath, Point};

use crate::lsp::Severity;

/// Half the wave's height, as a fraction of the row's.
const AMPLITUDE: f64 = 0.10;
/// One full period, as a fraction of the row's height. Tied to the row rather
/// than fixed in pixels so the wave keeps its proportions at any font size.
const WAVELENGTH: f64 = 0.34;
/// How far the wave's centre line sits above the bottom of the row, as a
/// fraction of the row's height. Enough that the wave clears the descenders it
/// runs under, and little enough that it stays inside its own row — a squiggle
/// that leaks into the line below reads as belonging to that line.
const SEAT: f64 = 0.14;

/// A visual row of text, as the squiggle layer sees it.
///
/// *Visual*, not logical: under soft wrap one line of the file is several of
/// these, and that is the entire reason this module exists.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualRow {
    /// The bytes of the document this row shows.
    pub offsets: Range<usize>,
    /// Document-space y of the row's top edge.
    pub top: f64,
    pub height: f64,
}

/// A diagnostic reduced to a document byte range.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticRange {
    pub offsets: Range<usize>,
    pub severity: Severity,
}

/// One run of squiggle: a byte span lying entirely within one visual row,
/// together with where to draw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Document byte range, clipped to a single row.
    pub offsets: Range<usize>,
    /// Document-space y of the wave's centre line.
    pub centre: f64,
    pub amplitude: f64,
    pub wavelength: f64,
    pub severity: Severity,
}

/// Split each diagnostic across the visual rows it covers.
///
/// Rows that are not on screen are simply not passed in, so a diagnostic
/// scrolled out of view produces nothing and costs nothing.
///
/// The result is ordered **least severe first**, so that painting it in order
/// leaves the worst diagnostic on top where two overlap. Two waves on the same
/// characters would otherwise interfere, and which one survived would depend on
/// the order the server happened to report them in.
pub fn runs(rows: &[VisualRow], diagnostics: &[DiagnosticRange]) -> Vec<Run> {
    let mut out = Vec::new();

    for diagnostic in diagnostics {
        for row in rows {
            let start = diagnostic.offsets.start.max(row.offsets.start);
            let end = diagnostic.offsets.end.min(row.offsets.end);
            if start >= end {
                // No overlap, or an overlap of nothing: a diagnostic ending
                // exactly where a row begins belongs to the row before it.
                continue;
            }
            out.push(Run {
                offsets: start..end,
                centre: row.top + row.height * (1.0 - SEAT),
                amplitude: row.height * AMPLITUDE,
                wavelength: row.height * WAVELENGTH,
                severity: diagnostic.severity,
            });
        }
    }

    out.sort_by_key(|run| loudness(run.severity));
    out
}

/// How bad a severity is, worse being higher.
///
/// Spelled out rather than derived from `Severity`'s declaration order, which
/// is an implementation detail of the LSP mapping and should not silently
/// decide what gets painted over what. `forged::circuit_reading` declines to
/// lean on it for the same reason.
fn loudness(severity: Severity) -> u8 {
    match severity {
        Severity::Hint => 0,
        Severity::Information => 1,
        Severity::Warning => 2,
        Severity::Error => 3,
    }
}

/// A wave along `x0..x1`, centred on `centre`.
///
/// Built from quadratic arcs, one per half period, alternating above and below.
/// A quadratic's midpoint sits halfway to its control point, so a control
/// offset of twice the amplitude gives a crest of exactly the amplitude.
///
/// The final half period is **truncated rather than overrun**, and its control
/// point is scaled down in proportion, so a run that is not a whole number of
/// periods ends exactly where the marked text ends with a shallower last bump
/// instead of a full crest hanging off the end. Runs are frequently short — a
/// point diagnostic is widened to a single character upstream, which is only
/// about one period wide — so the partial case is the common one, not the edge
/// case.
pub fn wave(x0: f64, x1: f64, centre: f64, amplitude: f64, wavelength: f64) -> BezPath {
    let mut path = BezPath::new();
    let width = x1 - x0;
    // Finiteness is checked, not assumed: these come from a text layout during
    // a resize, and a NaN would turn the loop below into a hang rather than a
    // wrong pixel.
    if !(width.is_finite() && width > 0.0 && wavelength.is_finite() && wavelength > 0.0) {
        return path;
    }

    let half = wavelength / 2.0;
    path.move_to(Point::new(x0, centre));

    let mut x = x0;
    let mut up = true;
    while x < x1 {
        let end = (x + half).min(x1);
        let completeness = (end - x) / half;
        let lift = if up { -1.0 } else { 1.0 } * 2.0 * amplitude * completeness;
        path.quad_to(
            Point::new((x + end) / 2.0, centre + lift),
            Point::new(end, centre),
        );
        x = end;
        up = !up;
    }

    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use floem::peniko::kurbo::Shape;

    fn row(offsets: Range<usize>, top: f64) -> VisualRow {
        VisualRow {
            offsets,
            top,
            height: 20.0,
        }
    }

    fn diagnostic(offsets: Range<usize>) -> DiagnosticRange {
        DiagnosticRange {
            offsets,
            severity: Severity::Error,
        }
    }

    /// The case the whole module exists for. A long line under soft wrap is
    /// several visual rows, and a diagnostic covering it has to become one run
    /// per row — each clipped to its own row, because a single rectangle
    /// spanning them would be drawn straight through the text in between.
    #[test]
    fn a_diagnostic_spanning_wrapped_rows_becomes_one_run_per_row() {
        let rows = [row(0..10, 0.0), row(10..20, 20.0), row(20..30, 40.0)];
        let got = runs(&rows, &[diagnostic(4..25)]);

        assert_eq!(got.len(), 3, "expected one run per row, got {got:?}");
        assert_eq!(got[0].offsets, 4..10);
        assert_eq!(got[1].offsets, 10..20);
        assert_eq!(got[2].offsets, 20..25);
    }

    /// A run that reached past its row would be handed to floem as an offset on
    /// a different row, and come back with an x belonging to that other row.
    #[test]
    fn a_run_never_reaches_outside_the_row_it_is_drawn_on() {
        let rows = [row(0..10, 0.0), row(10..20, 20.0)];
        for run in runs(&rows, &[diagnostic(0..100)]) {
            let on = rows
                .iter()
                .find(|r| r.offsets.contains(&run.offsets.start))
                .expect("every run belongs to a row");
            assert!(
                run.offsets.start >= on.offsets.start && run.offsets.end <= on.offsets.end,
                "run {:?} escapes its row {:?}",
                run.offsets,
                on.offsets
            );
        }
    }

    /// Scrolled out of view is not a special case to handle — it is rows that
    /// were never passed in.
    #[test]
    fn a_diagnostic_on_no_visible_row_draws_nothing() {
        let rows = [row(100..110, 0.0)];
        assert!(runs(&rows, &[diagnostic(0..50)]).is_empty());
    }

    /// A diagnostic ending exactly where the next row starts marks the row
    /// before it and not the one after. Without this a mark appears at column
    /// zero of the following row, which is the visible symptom of an
    /// off-by-one at a wrap boundary.
    #[test]
    fn a_diagnostic_ending_at_a_row_boundary_does_not_mark_the_next_row() {
        let rows = [row(0..10, 0.0), row(10..20, 20.0)];
        let got = runs(&rows, &[diagnostic(4..10)]);
        assert_eq!(got.len(), 1, "got {got:?}");
        assert_eq!(got[0].offsets, 4..10);
        assert_eq!(got[0].centre, rows[0].top + 20.0 * (1.0 - SEAT));
    }

    /// Two diagnostics on the same characters draw two waves in the same
    /// place. Which one the reader sees must be the worse of them, and must
    /// not depend on the order the server reported them in.
    #[test]
    fn an_error_is_painted_over_a_warning_on_the_same_characters() {
        let rows = [row(0..10, 0.0)];
        let warning = DiagnosticRange {
            offsets: 2..6,
            severity: Severity::Warning,
        };
        let error = DiagnosticRange {
            offsets: 2..6,
            severity: Severity::Error,
        };

        for order in [
            vec![warning.clone(), error.clone()],
            vec![error.clone(), warning.clone()],
        ] {
            let got = runs(&rows, &order);
            assert_eq!(got.len(), 2);
            assert_eq!(
                got.last().unwrap().severity,
                Severity::Error,
                "the error must be painted last, whatever order it arrived in"
            );
        }
    }

    /// A squiggle that leaks out of its row overlaps the text above or below
    /// it, and then reads as marking the wrong line.
    #[test]
    fn a_squiggle_stays_inside_the_row_it_marks() {
        let rows = [row(0..40, 100.0)];
        let run = runs(&rows, &[diagnostic(0..40)]).remove(0);
        let path = wave(0.0, 200.0, run.centre, run.amplitude, run.wavelength);
        let bounds = path.bounding_box();

        assert!(
            bounds.y0 >= rows[0].top,
            "the wave rises {:.2} above the top of its row",
            rows[0].top - bounds.y0
        );
        assert!(
            bounds.y1 <= rows[0].top + rows[0].height,
            "the wave drops {:.2} below the bottom of its row",
            bounds.y1 - (rows[0].top + rows[0].height)
        );
    }

    /// A wave that overran would underline characters that carry no
    /// diagnostic; one that stopped short would leave the last of them bare.
    #[test]
    fn a_wave_spans_exactly_the_text_it_underlines() {
        for width in [3.0_f64, 7.0, 40.0, 133.7] {
            let path = wave(10.0, 10.0 + width, 50.0, 2.0, 6.0);
            let bounds = path.bounding_box();
            assert!(
                (bounds.x0 - 10.0).abs() < 1e-9 && (bounds.x1 - (10.0 + width)).abs() < 1e-9,
                "a {width}px run drew {:.3}..{:.3}",
                bounds.x0,
                bounds.x1
            );
        }
    }

    /// The common case, not an edge case: a point diagnostic is widened to one
    /// character upstream, and one character is about one period wide. If a
    /// partial period drew flat, most diagnostics in practice would show a
    /// straight line rather than a squiggle.
    #[test]
    fn a_run_shorter_than_one_period_still_undulates() {
        let path = wave(0.0, 4.0, 50.0, 2.0, 12.0);
        let bounds = path.bounding_box();
        assert!(
            bounds.height() > 0.4,
            "a short run drew a {:.3}px-tall line, which is flat",
            bounds.height()
        );
    }

    /// The final half period is almost never a whole one, and a truncated bump
    /// given a full-height control point crams a full crest into whatever
    /// fraction of a period is left — a spike on the end of an otherwise even
    /// wave, steeper than every bump before it. Scaling the control point by
    /// how much of the period survived is what keeps the last bump in
    /// proportion to the space it has.
    ///
    /// Added after a mutation check: deleting that scaling broke nothing, so
    /// nothing was guarding it.
    #[test]
    fn a_truncated_final_bump_is_shallower_than_a_whole_one() {
        let (centre, amplitude, wavelength) = (50.0, 2.0, 12.0);
        let whole = wave(0.0, 6.0, centre, amplitude, wavelength).bounding_box();
        let tenth = wave(0.0, 0.6, centre, amplitude, wavelength).bounding_box();

        assert!(
            (whole.height() - amplitude).abs() < 1e-9,
            "a whole half period should crest at exactly the amplitude, got {:.3}",
            whole.height()
        );
        assert!(
            tenth.height() < whole.height() * 0.25,
            "a tenth of a period crested {:.3}, nearly as high as a whole one at \
             {:.3} — that is a spike",
            tenth.height(),
            whole.height()
        );
    }

    /// Degenerate inputs reach here from real geometry — a run clipped to
    /// nothing by a scroll, or a zero-height row during layout — and must not
    /// hang or panic.
    #[test]
    fn a_wave_with_nothing_to_draw_draws_nothing() {
        assert!(wave(10.0, 10.0, 0.0, 2.0, 6.0).elements().is_empty());
        assert!(wave(10.0, 0.0, 0.0, 2.0, 6.0).elements().is_empty());
        assert!(wave(0.0, 10.0, 0.0, 2.0, 0.0).elements().is_empty());
    }
}

//! Hierarchical 2D layout of the palette editor sheet.
//!
//! Entries are ordered from global chrome down to domain tracks and grouped onto shared rows so
//! related colours (plain/mid-zoom tinted pairs, A/B alternations, fg/bg pairs) sit side by side.
//! Every entry references `palette::idx::*` directly so renames or removals fail to compile, and
//! unit tests verify that `SECTIONS` covers `palette::REGISTRY` bijectively and fits `PANE_WIDTH`.

use crate::style::palette::idx;

/// Columns per swatch on the sheet and picker sibling strip.
///
/// Two cells so each colour reads as an area rather than a glyph while keeping the 12-swatch
/// `Bases` row (6 pairs + 5 gaps + indent = 31 cols) inside `PANE_WIDTH`.
pub(super) const SWATCH_WIDTH: usize = 2;

/// Leading columns before a swatch strip.
pub(super) const STRIP_INDENT: usize = 2;

/// Blank columns separating distinct clusters on the same row.
pub(super) const CLUSTER_GAP: usize = 1;

/// One editable swatch on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Swatch {
    /// Index into `palette::REGISTRY` / `LIVE`.
    pub(super) entry: usize,
    /// Short human-readable description shown in the pinned header when focused.
    pub(super) label: &'static str,
}

impl Swatch {
    const fn new(entry: usize, label: &'static str) -> Self {
        Self { entry, label }
    }
}

/// Tightly adjacent swatches rendered with no gap between them (e.g. plain + mid-zoom tint).
pub(super) type Cluster = &'static [Swatch];

/// One navigable row of related swatches, rendered as a label line followed by its swatch strip.
#[derive(Debug, Clone, Copy)]
pub(super) struct SwatchRow {
    pub(super) label: &'static str,
    pub(super) clusters: &'static [Cluster],
}

impl SwatchRow {
    /// Total number of swatches on this row.
    pub(super) fn len(&self) -> usize {
        self.clusters.iter().map(|cluster| cluster.len()).sum()
    }

    /// The `col`-th swatch on this row, flattened across clusters.
    pub(super) fn swatch(&self, col: usize) -> Swatch {
        let mut remaining = col;
        for cluster in self.clusters {
            if remaining < cluster.len() {
                return cluster[remaining];
            }
            remaining -= cluster.len();
        }
        panic!("swatch column {col} out of bounds for row {:?}", self.label);
    }

    /// Total terminal columns occupied by this row's swatch strip, including its indent.
    #[cfg(test)]
    pub(super) fn display_width(&self) -> usize {
        STRIP_INDENT + self.len() * SWATCH_WIDTH + self.clusters.len().saturating_sub(1) * CLUSTER_GAP
    }

    /// Starting terminal column of the `col`-th swatch on this row's swatch strip.
    #[cfg(test)]
    pub(super) fn swatch_x(&self, col: usize) -> usize {
        let mut x = STRIP_INDENT;
        let mut remaining = col;
        for (cluster_idx, cluster) in self.clusters.iter().enumerate() {
            if cluster_idx > 0 {
                x += CLUSTER_GAP;
            }
            if remaining < cluster.len() {
                return x + remaining * SWATCH_WIDTH;
            }
            x += cluster.len() * SWATCH_WIDTH;
            remaining -= cluster.len();
        }
        panic!("swatch column {col} out of bounds for row {:?}", self.label);
    }
}

/// A non-selectable heading followed by its swatch rows.
#[derive(Debug, Clone, Copy)]
pub(super) struct Section {
    pub(super) title: &'static str,
    pub(super) rows: &'static [SwatchRow],
}

pub(super) const SECTIONS: &[Section] = &[
    Section {
        title: "Chrome & Surfaces",
        rows: &[
            SwatchRow {
                label: "Surfaces",
                clusters: &[
                    &[Swatch::new(idx::Surface::APP_BG, "App & bars")],
                    &[Swatch::new(idx::Surface::TRACK_BG, "Tracks")],
                    &[Swatch::new(idx::Surface::PANEL_BG, "Panels / tables")],
                    &[Swatch::new(idx::Surface::POPOVER_BG, "Popovers")],
                    &[Swatch::new(idx::Surface::LABEL_BG, "Labels / headers")],
                    &[Swatch::new(idx::Chrome::OUT_OF_BOUNDS_BG, "Out of bounds")],
                ],
            },
            SwatchRow {
                label: "Text",
                clusters: &[
                    &[Swatch::new(idx::Text::PRIMARY_FG, "Primary")],
                    &[Swatch::new(idx::Text::DIMMED_FG, "Dimmed")],
                    &[Swatch::new(idx::Text::ON_STATUS_FG, "On status")],
                    &[Swatch::new(idx::Text::ON_DATA_FG, "On data")],
                ],
            },
            SwatchRow {
                label: "Selection",
                clusters: &[
                    &[Swatch::new(idx::Selection::FOCUS_BG, "Focus")],
                    &[Swatch::new(idx::Selection::CONTEXT_BG, "Context / crosshair")],
                    &[Swatch::new(idx::Selection::ACTIVE_TAB_BG, "Active tab")],
                    &[Swatch::new(idx::Selection::INPUT_BG, "Command line")],
                ],
            },
            SwatchRow {
                label: "Rules & marks",
                clusters: &[
                    &[Swatch::new(idx::Chrome::DIVIDER_FG, "Dividers")],
                    &[Swatch::new(idx::Chrome::RULER_BAND_BG, "Ruler band")],
                    &[Swatch::new(idx::Chrome::ACTIVITY_FG, "Activity spinner")],
                    &[Swatch::new(idx::Chrome::MARKER_FG, "Extent marker")],
                ],
            },
            SwatchRow {
                label: "Status",
                clusters: &[
                    &[Swatch::new(idx::Status::ONGOING_BG, "Ongoing")],
                    &[Swatch::new(idx::Status::OK_BG, "OK / info")],
                    &[Swatch::new(idx::Status::WARN_BG, "Warning")],
                    &[Swatch::new(idx::Status::ERROR_BG, "Error")],
                ],
            },
        ],
    },
    Section {
        title: "Bases & Indels",
        rows: &[
            SwatchRow {
                label: "Bases",
                clusters: &[
                    &[
                        Swatch::new(idx::Reference::BASE_A, "A"),
                        Swatch::new(idx::Reference::BASE_A_TINTED, "A tinted"),
                    ],
                    &[
                        Swatch::new(idx::Reference::BASE_C, "C"),
                        Swatch::new(idx::Reference::BASE_C_TINTED, "C tinted"),
                    ],
                    &[
                        Swatch::new(idx::Reference::BASE_G, "G"),
                        Swatch::new(idx::Reference::BASE_G_TINTED, "G tinted"),
                    ],
                    &[
                        Swatch::new(idx::Reference::BASE_T, "T"),
                        Swatch::new(idx::Reference::BASE_T_TINTED, "T tinted"),
                    ],
                    &[
                        Swatch::new(idx::Reference::BASE_N, "N"),
                        Swatch::new(idx::Reference::BASE_N_TINTED, "N tinted"),
                    ],
                    &[
                        Swatch::new(idx::Reference::BASE_NULL, "Null"),
                        Swatch::new(idx::Reference::BASE_NULL_TINTED, "Null tinted"),
                    ],
                ],
            },
            SwatchRow {
                label: "Indels",
                clusters: &[
                    &[Swatch::new(idx::Indel::DELETION, "Deletion")],
                    &[Swatch::new(idx::Indel::INSERTION, "Insertion")],
                ],
            },
        ],
    },
    Section {
        title: "Alignment Pileup",
        rows: &[
            SwatchRow {
                label: "Lanes",
                clusters: &[
                    &[
                        Swatch::new(idx::Alignment::PILEUP_LANE_A, "Lane A"),
                        Swatch::new(idx::Alignment::PILEUP_LANE_A_TINTED, "Lane A tinted"),
                    ],
                    &[
                        Swatch::new(idx::Alignment::PILEUP_LANE_B, "Lane B"),
                        Swatch::new(idx::Alignment::PILEUP_LANE_B_TINTED, "Lane B tinted"),
                    ],
                ],
            },
            SwatchRow {
                label: "Strands",
                clusters: &[
                    &[Swatch::new(idx::Alignment::READ_FORWARD_BG, "Forward")],
                    &[Swatch::new(idx::Alignment::READ_REVERSE_BG, "Reverse")],
                ],
            },
            SwatchRow {
                label: "Centerline",
                clusters: &[&[
                    Swatch::new(idx::Alignment::HIGHLIGHT_A, "Highlight A"),
                    Swatch::new(idx::Alignment::HIGHLIGHT_B, "Highlight B"),
                ]],
            },
            SwatchRow {
                label: "Details",
                clusters: &[
                    &[Swatch::new(idx::Alignment::ARROW_FG, "Direction arrows")],
                    &[Swatch::new(idx::Alignment::UNKNOWN_MISMATCH, "Unknown mismatch")],
                ],
            },
            SwatchRow {
                label: "5mC methylation",
                clusters: &[&[
                    Swatch::new(idx::Alignment::MOD_5MC_HYPO, "Hypo"),
                    Swatch::new(idx::Alignment::MOD_5MC_UNCERTAIN, "Uncertain"),
                    Swatch::new(idx::Alignment::MOD_5MC_HYPER, "Hyper"),
                ]],
            },
        ],
    },
    Section {
        title: "Coverage, Ruler & Features",
        rows: &[
            SwatchRow {
                label: "Coverage & context",
                clusters: &[
                    &[Swatch::new(idx::Coverage::FG, "Coverage")],
                    &[Swatch::new(
                        idx::LocalArea::BREAKPOINT_COVERAGE_FG,
                        "Breakpoint coverage",
                    )],
                    &[Swatch::new(idx::LocalArea::VIEWPORT_MARKER_FG, "Viewport marker")],
                ],
            },
            SwatchRow {
                label: "5mC coverage",
                clusters: &[&[
                    Swatch::new(idx::Coverage::BG, "Background"),
                    Swatch::new(idx::LocalArea::MOD_5MC_NO_DATA, "No data"),
                ]],
            },
            SwatchRow {
                label: "Ruler",
                clusters: &[&[Swatch::new(idx::Coordinate::NUMBERING_FG, "Coordinate numbers")]],
            },
            SwatchRow {
                label: "Features",
                clusters: &[
                    &[Swatch::new(idx::Features::EXON_BG, "Exon fill")],
                    &[Swatch::new(idx::Features::INTRON_FG, "Intron line")],
                ],
            },
        ],
    },
    Section {
        title: "Contigs & Minimap",
        rows: &[
            SwatchRow {
                label: "Contig pills",
                clusters: &[
                    &[
                        Swatch::new(idx::Reference::CONTIG_A_BG, "Contig A"),
                        Swatch::new(idx::Reference::CONTIG_A_HIGHLIGHT_BG, "Contig A active"),
                    ],
                    &[
                        Swatch::new(idx::Reference::CONTIG_B_BG, "Contig B"),
                        Swatch::new(idx::Reference::CONTIG_B_HIGHLIGHT_BG, "Contig B active"),
                    ],
                ],
            },
            SwatchRow {
                label: "Contig bars",
                clusters: &[
                    &[
                        Swatch::new(idx::Chrome::CONTIG_BAR_A, "Bar A"),
                        Swatch::new(idx::Chrome::CONTIG_BAR_B, "Bar B"),
                    ],
                    &[
                        Swatch::new(idx::Chrome::CONTIG_BAR_DIMMED_A, "Bar A dimmed"),
                        Swatch::new(idx::Chrome::CONTIG_BAR_DIMMED_B, "Bar B dimmed"),
                    ],
                ],
            },
            SwatchRow {
                label: "Minimap",
                clusters: &[
                    &[
                        Swatch::new(idx::Minimap::CONTIG_BAR_BG, "Bar bg"),
                        Swatch::new(idx::Minimap::CONTIG_BAR_FG, "Bar fg"),
                    ],
                    &[
                        Swatch::new(idx::Minimap::CONTIG_BAR_HIGHLIGHT_BG, "Highlight bg"),
                        Swatch::new(idx::Minimap::CONTIG_BAR_HIGHLIGHT_FG, "Highlight fg"),
                    ],
                ],
            },
        ],
    },
];

/// Total navigable swatch rows across all sections.
pub(super) fn row_count() -> usize {
    SECTIONS.iter().map(|section| section.rows.len()).sum()
}

/// Returns the `flat_row`-th navigable swatch row across all sections.
pub(super) fn row_at(flat_row: usize) -> &'static SwatchRow {
    let mut remaining = flat_row;
    for section in SECTIONS {
        if remaining < section.rows.len() {
            return &section.rows[remaining];
        }
        remaining -= section.rows.len();
    }
    panic!("palette row {flat_row} out of bounds");
}

/// Total rendered lines in the scrollable body (1 line per section heading + 2 lines per row).
pub(super) fn total_body_lines() -> usize {
    SECTIONS.iter().map(|section| 1 + section.rows.len() * 2).sum()
}

/// Line span `(reveal_top, row_bottom)` in the scrollable body for `flat_row`.
///
/// When `flat_row` is the first row of its section, `reveal_top` includes that section's heading
/// so scrolling up to a section reveals its title alongside its first row.
pub(super) fn sheet_line_span(flat_row: usize) -> (usize, usize) {
    let mut line = 0;
    let mut row_index = 0;
    for section in SECTIONS {
        let heading_line = line;
        line += 1;
        for (local_idx, _) in section.rows.iter().enumerate() {
            let label_line = line;
            let strip_line = line + 1;
            if row_index == flat_row {
                let reveal_top = if local_idx == 0 { heading_line } else { label_line };
                return (reveal_top, strip_line);
            }
            line += 2;
            row_index += 1;
        }
    }
    panic!("palette row {flat_row} out of bounds");
}

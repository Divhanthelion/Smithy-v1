//! Pending file change management for reviewable diffs
//!
//! `WriteReviewHook` intercepts `write` and `edit`, computes the resulting diff,
//! and queues a [`PendingFileChange`] instead of letting the tool touch disk.
//! The diff modal then shows it and the user accepts or skips each hunk;
//! [`content_with_accepted_hunks`] turns those decisions into file content.
//!
//! The decision model is deliberately conservative: [`ChangeStatus::Pending`]
//! means *do not write*. An unreviewed change can therefore never leak onto
//! disk, and the intent to apply has to be supplied explicitly by whoever is
//! looking at the diff.

use crate::diff_view::FileDiff;

/// A per-hunk review decision.
///
/// `Pending` means *undecided*, and every consumer treats undecided as **do not
/// write** — see [`content_with_accepted_hunks`]. That is what keeps an
/// unreviewed change from ever reaching disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Pending,
    Accepted,
    Rejected,
}

/// A queued write, waiting for the user to review it.
///
/// Deliberately just an id and a diff. It previously also carried `path`,
/// `old_content` and `new_content` — all three of which [`FileDiff`] already
/// holds — plus a `status` and a `hunk_statuses` vector that nothing outside its
/// own tests ever read.
///
/// The per-hunk decisions are **not** stored here on purpose. They belong to the
/// modal displaying the change, and a second copy on this side is precisely how
/// the tab bar and the editor once came to disagree about dirty state.
#[derive(Debug, Clone)]
pub struct PendingFileChange {
    /// The `tool_call_id` this change came from. Reviews are matched on it, never
    /// on the path — one turn can queue two writes to the same file.
    pub id: String,
    /// The change itself, including the path and both sides of the content.
    pub diff: FileDiff,
}

impl PendingFileChange {
    /// Create a new pending change from old and new content.
    pub fn new(
        id: impl Into<String>,
        path: impl Into<String>,
        old_content: String,
        new_content: String,
    ) -> Self {
        let path = path.into();
        Self {
            id: id.into(),
            diff: FileDiff::from_content(&path, &old_content, &new_content),
        }
    }

    /// The path under review, relative to the workspace root.
    pub fn path(&self) -> &str {
        &self.diff.path
    }
}

/// Apply a per-hunk decision vector to a diff, returning the resulting content.
///
/// Walks the old file once, splicing in each accepted hunk's new lines and
/// copying the old lines through for every hunk that was rejected or is still
/// pending. Hunks are disjoint and ordered by old line — that is what
/// `similar`'s `grouped_ops` guarantees — so a single forward cursor is enough
/// and no offset bookkeeping is needed.
///
/// Lines keep their terminators throughout, so accepting nothing returns the
/// old content byte-for-byte and accepting everything returns the new content
/// byte-for-byte, including a missing final newline. An earlier version
/// returned the full new content for *any* partial acceptance, which meant
/// rejecting a hunk silently applied it anyway.
///
/// `statuses` shorter than `hunks` leaves the remaining hunks unapplied, which
/// is the safe direction: an undecided hunk is never written.
pub fn content_with_accepted_hunks(diff: &FileDiff, statuses: &[ChangeStatus]) -> String {
    use crate::diff_view::split_keeping_ends;

    let old_lines = split_keeping_ends(&diff.old_content);
    let new_lines = split_keeping_ends(&diff.new_content);

    let mut out = String::with_capacity(diff.new_content.len());
    let mut cursor = 0usize;

    for (idx, hunk) in diff.hunks.iter().enumerate() {
        let old_range = hunk.old_lines();
        // Defensive, not expected: a hunk that ran past the end of the old file
        // would mean the diff and the content had come apart. Emitting the old
        // content unchanged is the safe reading — never invent a write the user
        // did not approve.
        if old_range.start < cursor || old_range.end > old_lines.len() {
            return diff.old_content.clone();
        }

        out.extend(old_lines[cursor..old_range.start].iter().copied());
        if statuses.get(idx) == Some(&ChangeStatus::Accepted) {
            let new_range = hunk.new_lines();
            if new_range.end > new_lines.len() {
                return diff.old_content.clone();
            }
            out.extend(new_lines[new_range].iter().copied());
        } else {
            out.extend(old_lines[old_range.clone()].iter().copied());
        }
        cursor = old_range.end;
    }

    out.extend(old_lines[cursor..].iter().copied());
    out
}

/// The queue of writes awaiting review.
///
/// A change leaves the queue by being [`remove`](Self::remove)d — there is no
/// per-change status to mark. An earlier version filtered [`queued`](Self::queued)
/// on a `status` field that nothing ever wrote, so the filter always passed;
/// leaving it in place invited a later reader to "fix" it and silently change
/// what the queue means.
#[derive(Debug, Default)]
pub struct PendingChangeManager {
    changes: Vec<PendingFileChange>,
}

impl PendingChangeManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a change for review.
    pub fn add(&mut self, change: PendingFileChange) {
        self.changes.push(change);
    }

    /// Take a change out of the queue by its tool-call id.
    pub fn remove(&mut self, id: &str) -> Option<PendingFileChange> {
        let idx = self.changes.iter().position(|c| c.id == id)?;
        Some(self.changes.remove(idx))
    }

    /// Everything still awaiting review, in the order it was queued.
    pub fn queued(&self) -> &[PendingFileChange] {
        &self.changes
    }

    /// Abandon every queued review.
    ///
    /// For a project switch. A queued change carries a workspace-*relative*
    /// path and is written through whichever root is live when it is accepted,
    /// so one that outlives its project resolves into the next one — which is
    /// how a `README.md` proposed in one repository lands in another.
    pub fn clear(&mut self) {
        self.changes.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructing a change derives the diff from the two sides — the path and
    /// both contents live in the diff, not alongside it.
    #[test]
    fn a_change_carries_its_id_and_derives_its_diff() {
        let change = PendingFileChange::new(
            "call_1",
            "test.rs",
            "fn main() {}\n".to_string(),
            "fn main() {\n    println!(\"hello\");\n}\n".to_string(),
        );

        assert_eq!(change.id, "call_1");
        assert_eq!(change.path(), "test.rs");
        assert!(change.diff.has_changes());
    }

    /// Reviews are matched on the tool-call id, not the path: one turn can queue
    /// two writes to the same file, and removing "the one for a.rs" would then
    /// drop the wrong review.
    #[test]
    fn two_changes_to_one_path_are_queued_and_removed_independently() {
        let mut mgr = PendingChangeManager::new();
        mgr.add(PendingFileChange::new(
            "c1",
            "a.rs",
            String::new(),
            "first".into(),
        ));
        mgr.add(PendingFileChange::new(
            "c2",
            "a.rs",
            String::new(),
            "second".into(),
        ));

        assert_eq!(mgr.queued().len(), 2);

        let removed = mgr.remove("c1").expect("c1 was queued");
        assert_eq!(removed.id, "c1");
        assert_eq!(mgr.queued().len(), 1);
        assert_eq!(mgr.queued()[0].id, "c2");

        assert!(mgr.remove("c1").is_none(), "removing twice is not an error");
        assert!(mgr.remove("c2").is_some());
        assert!(mgr.is_empty());
    }

    /// A diff with two well-separated edits, so it has two hunks.
    fn two_hunk_diff() -> FileDiff {
        let old: String = (0..40)
            .map(|i| format!("line{i}\n"))
            .collect::<Vec<_>>()
            .join("");
        let mut new_lines: Vec<String> = (0..40).map(|i| format!("line{i}\n")).collect();
        new_lines[3] = "EARLY\n".into();
        new_lines[34] = "LATE\n".into();
        let new: String = new_lines.join("");

        let diff = FileDiff::from_content("t.rs", &old, &new);
        assert_eq!(diff.hunks.len(), 2, "fixture must have two hunks");
        diff
    }

    /// The defect this replaced: rejecting a hunk applied it anyway, because
    /// any partial acceptance returned the whole new content.
    #[test]
    fn a_rejected_hunk_is_not_written() {
        let diff = two_hunk_diff();
        let result =
            content_with_accepted_hunks(&diff, &[ChangeStatus::Accepted, ChangeStatus::Rejected]);

        assert!(result.contains("EARLY\n"), "the accepted hunk must land");
        assert!(
            !result.contains("LATE\n"),
            "the rejected hunk must not land — this is the whole point of per-hunk review"
        );
        assert!(
            result.contains("line34\n"),
            "the rejected hunk's original line must survive"
        );
    }

    /// The mirror image, so the test cannot pass by always keeping the old text.
    #[test]
    fn an_accepted_hunk_is_written() {
        let diff = two_hunk_diff();
        let result =
            content_with_accepted_hunks(&diff, &[ChangeStatus::Rejected, ChangeStatus::Accepted]);

        assert!(!result.contains("EARLY\n"));
        assert!(result.contains("LATE\n"));
        assert!(result.contains("line3\n"));
    }

    /// Accepting or rejecting everything must reproduce the endpoints exactly —
    /// byte-for-byte, not merely line-for-line.
    #[test]
    fn accepting_all_or_none_reproduces_the_endpoints_byte_for_byte() {
        let diff = two_hunk_diff();

        assert_eq!(
            content_with_accepted_hunks(&diff, &[ChangeStatus::Accepted; 2]),
            diff.new_content
        );
        assert_eq!(
            content_with_accepted_hunks(&diff, &[ChangeStatus::Rejected; 2]),
            diff.old_content
        );
    }

    /// A file whose last line has no newline must not silently acquire one.
    /// Reassembling from `lines()` rather than `split_inclusive` would.
    #[test]
    fn a_missing_final_newline_is_preserved() {
        let diff = FileDiff::from_content("t.rs", "alpha\nbravo\ncharlie", "alpha\nBRAVO\ncharlie");
        let result =
            content_with_accepted_hunks(&diff, &vec![ChangeStatus::Accepted; diff.hunks.len()]);

        assert_eq!(result, "alpha\nBRAVO\ncharlie");
        assert!(!result.ends_with('\n'), "no newline was added to the file");
    }

    /// An undecided hunk keeps the old text: accepting one hunk must not quietly
    /// apply the rest, and a statuses vector shorter than the hunk list must not
    /// be read as blanket approval.
    #[test]
    fn an_undecided_hunk_keeps_the_old_content() {
        let diff = two_hunk_diff();

        let explicit =
            content_with_accepted_hunks(&diff, &[ChangeStatus::Accepted, ChangeStatus::Pending]);
        assert!(explicit.contains("EARLY\n"));
        assert!(!explicit.contains("LATE\n"), "hunk 1 was never decided");

        let truncated = content_with_accepted_hunks(&diff, &[ChangeStatus::Accepted]);
        assert_eq!(
            truncated, explicit,
            "a missing decision must read the same as an undecided one"
        );
    }

    /// A brand-new file has no old content to interleave with.
    #[test]
    fn a_new_file_is_written_whole_when_accepted() {
        let diff = FileDiff::from_content("new.rs", "", "fn main() {}\n");

        assert_eq!(
            content_with_accepted_hunks(&diff, &vec![ChangeStatus::Accepted; diff.hunks.len()]),
            "fn main() {}\n"
        );
        assert_eq!(
            content_with_accepted_hunks(&diff, &vec![ChangeStatus::Rejected; diff.hunks.len()]),
            ""
        );
    }
}

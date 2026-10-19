//! Where Go's variables are visible. Go describes a local's scope by its
//! block's code, which unoptimized builds do not divide into blocks, so a
//! local would seem to exist before its declaration has run. As Delve
//! does, a Go local is visible only where the line executing is past the
//! line declaring it, and a block's code includes its nested blocks'.
//!
//! The line executing at an address is the site of the call inlined
//! directly into the local's scope that contains the address, and
//! otherwise the line of the code itself. It is asked of the image at the
//! address queried, so loading computes nothing per local.

use crate::image::Image;
use crate::image::functions::FunctionView;
use crate::image::lines::LineView;
use crate::{AddressRange, CodeInstanceId, CodeInstanceKind, ImageAddress, SourceLocation};

pub(super) use crate::image::variables::GoDeclaration;

/// Whether a local declared at `declaration` is visible where `line` is
/// executing: past the declaration's line in its file. Code whose line is
/// unknown, or in another file, stays visible: hiding it would be a guess.
pub(super) fn shown(declaration: &SourceLocation, line: Option<&SourceLocation>) -> bool {
    line.is_none_or(|line| line.file != declaration.file || line.line > declaration.line)
}

/// A call inlined into a scope, which stands for its site.
pub(super) struct InlinedCall {
    /// Where it was called from, when that is known.
    pub(super) site: Option<SourceLocation>,
}

/// What decides which line is executing.
pub(super) trait Code {
    /// The first call, in instance order, inlined directly into `parent`
    /// whose code contains `address`.
    fn call_at(&self, parent: CodeInstanceId, address: ImageAddress) -> Option<InlinedCall>;

    /// The line of the code at `address`.
    fn line_at(&self, address: ImageAddress) -> Option<SourceLocation>;

    /// The addresses in `range` where the line or the call inlined into
    /// `parent` may change.
    fn boundaries(
        &self,
        parent: Option<CodeInstanceId>,
        range: AddressRange<ImageAddress>,
    ) -> Vec<ImageAddress>;
}

/// The line executing at `address` in the scope of `declared`: the site
/// of the call inlined directly into the scope's instance that contains
/// the address, and otherwise the line of the code.
pub(super) fn executing(
    code: &dyn Code,
    declared: &GoDeclaration,
    address: ImageAddress,
) -> Option<SourceLocation> {
    declared
        .instance
        .and_then(|parent| code.call_at(parent, address))
        .map_or_else(|| code.line_at(address), |call| call.site)
}

/// Whether a local declared as `declared` is visible at `address`, which
/// its scope's code contains.
pub(super) fn visible_at(code: &dyn Code, declared: &GoDeclaration, address: ImageAddress) -> bool {
    shown(
        &declared.location,
        executing(code, declared, address).as_ref(),
    )
}

/// The parts of `ranges` where a local declared as `declared` is visible.
pub(super) fn visible_ranges(
    code: &dyn Code,
    ranges: &[AddressRange<ImageAddress>],
    declared: &GoDeclaration,
) -> Vec<AddressRange<ImageAddress>> {
    let boundaries = ranges
        .iter()
        .flat_map(|range| code.boundaries(declared.instance, *range))
        .collect::<Vec<_>>();

    segments(ranges, &boundaries, |address| {
        visible_at(code, declared, address)
    })
}

impl Code for Image {
    fn call_at(&self, parent: CodeInstanceId, address: ImageAddress) -> Option<InlinedCall> {
        let call = FunctionView::new(self)
            .instances_containing(address)
            .filter(|instance| instance.parent() == Some(parent))
            .min_by_key(|instance| instance.id())?;

        match call.kind() {
            CodeInstanceKind::Inline { call_site } => Some(InlinedCall { site: call_site }),
            CodeInstanceKind::OutOfLine => None,
        }
    }

    fn line_at(&self, address: ImageAddress) -> Option<SourceLocation> {
        LineView::new(self)
            .line_entry_containing(address)
            .map(|entry| entry.location)
    }

    fn boundaries(
        &self,
        parent: Option<CodeInstanceId>,
        range: AddressRange<ImageAddress>,
    ) -> Vec<ImageAddress> {
        let lines = LineView::new(self);
        let mut boundaries = lines
            .line_entry_containing(range.start)
            .into_iter()
            .chain(lines.line_entries_starting_in([range]))
            .flat_map(|entry| [entry.range.start, entry.range.end])
            .collect::<Vec<_>>();

        if let Some(parent) = parent {
            boundaries.extend(
                FunctionView::new(self)
                    .instances()
                    .filter(|instance| instance.parent() == Some(parent))
                    .flat_map(crate::image::functions::CodeInstance::ranges)
                    .flat_map(|call| [call.start, call.end]),
            );
        }

        boundaries
    }
}

/// The parts of `ranges` where `visible` holds, which may change only at
/// `boundaries`, merged where they meet.
pub(super) fn segments(
    ranges: &[AddressRange<ImageAddress>],
    boundaries: &[ImageAddress],
    visible: impl Fn(ImageAddress) -> bool,
) -> Vec<AddressRange<ImageAddress>> {
    let mut visible_ranges = Vec::<AddressRange<ImageAddress>>::new();
    for range in ranges {
        let mut cuts = boundaries
            .iter()
            .copied()
            .filter(|address| range.start < *address && *address < range.end)
            .chain([range.start, range.end])
            .collect::<Vec<_>>();
        cuts.sort_unstable();
        cuts.dedup();

        for segment in cuts.windows(2) {
            let (start, end) = (segment[0], segment[1]);
            if !visible(start) {
                continue;
            }

            match visible_ranges.last_mut() {
                Some(last) if last.end == start => last.end = end,
                _ => visible_ranges.push(AddressRange { start, end }),
            }
        }
    }

    visible_ranges
}

/// Ranges merged into the fewest sorted ranges covering them.
pub(super) fn fused(
    mut ranges: Vec<AddressRange<ImageAddress>>,
) -> Vec<AddressRange<ImageAddress>> {
    ranges.sort_by_key(|range| range.start);
    let mut fused = Vec::<AddressRange<ImageAddress>>::with_capacity(ranges.len());
    for range in ranges {
        match fused.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => fused.push(range),
        }
    }

    fused
}

#[cfg(test)]
mod tests;

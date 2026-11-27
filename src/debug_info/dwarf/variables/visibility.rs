//! Go declaration-line variable visibility filtering.
//!
//! Go omits `DW_AT_start_scope` on locals (scoping each to its whole enclosing
//! block), so locals are hidden until execution advances past their declaration
//! line (using the inlined call site when stopped inside an inlined callee).

use crate::image::Image;
use crate::image::functions::FunctionView;
use crate::image::lines::LineView;
use crate::{AddressRange, CodeInstanceId, CodeInstanceKind, ImageAddress, SourceLocation};

pub(super) use crate::image::variables::GoDeclaration;

pub(super) fn shown(declaration: &SourceLocation, line: Option<&SourceLocation>) -> bool {
    line.is_none_or(|line| line.file != declaration.file || line.line > declaration.line)
}

pub(super) struct InlinedCall {
    pub(super) site: Option<SourceLocation>,
}

pub(super) trait Code {
    fn call_at(&self, parent: CodeInstanceId, address: ImageAddress) -> Option<InlinedCall>;

    fn line_at(&self, address: ImageAddress) -> Option<SourceLocation>;

    fn boundaries(
        &self,
        parent: Option<CodeInstanceId>,
        range: AddressRange<ImageAddress>,
    ) -> Vec<ImageAddress>;
}

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

pub(super) fn visible_at(code: &dyn Code, declared: &GoDeclaration, address: ImageAddress) -> bool {
    shown(
        &declared.location,
        executing(code, declared, address).as_ref(),
    )
}

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

        for (&start, &end) in cuts.iter().zip(cuts.iter().skip(1)) {
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

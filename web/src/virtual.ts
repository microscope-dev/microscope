import {
  elementScroll,
  observeElementOffset,
  observeElementRect,
  type PartialKeys,
  Virtualizer,
  type VirtualItem,
  type VirtualizerOptions,
} from "@tanstack/virtual-core";
import { createEffect, createMemo, createSignal, onSettled, untrack } from "solid-js";

export type { VirtualItem, Virtualizer };

export type SolidVirtualizerOptions<
  TScrollElement extends Element,
  TItemElement extends Element = Element,
> = PartialKeys<
  VirtualizerOptions<TScrollElement, TItemElement>,
  "observeElementRect" | "observeElementOffset" | "scrollToFn"
>;

export interface SolidVirtualizer<
  TScrollElement extends Element,
  TItemElement extends Element = Element,
> {
  readonly instance: Virtualizer<TScrollElement, TItemElement>;
  items(): VirtualItem[];
  totalSize(): number;
  scrollToIndex: Virtualizer<TScrollElement, TItemElement>["scrollToIndex"];
  scrollToOffset: Virtualizer<TScrollElement, TItemElement>["scrollToOffset"];
  measureElement: Virtualizer<TScrollElement, TItemElement>["measureElement"];
}

export function createVirtualizer<
  TScrollElement extends Element,
  TItemElement extends Element = Element,
>(
  options: () => SolidVirtualizerOptions<TScrollElement, TItemElement>,
): SolidVirtualizer<TScrollElement, TItemElement> {
  const [tick, setTick] = createSignal(0);

  const resolve = (
    opts: SolidVirtualizerOptions<TScrollElement, TItemElement>,
  ): VirtualizerOptions<TScrollElement, TItemElement> => ({
    observeElementRect,
    observeElementOffset,
    scrollToFn: elementScroll,
    ...opts,
    onChange: (virtualizer, sync) => {
      setTick((n) => n + 1);
      opts.onChange?.(virtualizer, sync);
    },
  });

  const instance = new Virtualizer<TScrollElement, TItemElement>(resolve(untrack(options)));

  let mountedEl: TScrollElement | null = null;
  let unmountEl: (() => void) | undefined;

  const syncElement = (el: TScrollElement | null) => {
    if (el !== mountedEl) {
      unmountEl?.();
      mountedEl = el;
      // oxlint-disable-next-line no-underscore-dangle -- @tanstack/virtual-core lifecycle method
      unmountEl = el ? instance._didMount() : undefined;
    }
  };

  const resolved = createMemo(() => {
    const next = resolve(options());
    instance.setOptions(next);
    return next;
  });

  createEffect(
    () => resolved(),
    (next) => {
      syncElement(untrack(() => next.getScrollElement()));
      // oxlint-disable-next-line no-underscore-dangle -- @tanstack/virtual-core lifecycle method
      untrack(() => instance._willUpdate());
      setTick((n) => n + 1);
    },
  );

  onSettled(() => {
    const el = untrack(() => instance.options.getScrollElement());
    if (el && el !== mountedEl) {
      syncElement(el);
      // oxlint-disable-next-line no-underscore-dangle -- @tanstack/virtual-core lifecycle method
      untrack(() => instance._willUpdate());
      setTick((n) => n + 1);
    }
    return () => {
      unmountEl?.();
      unmountEl = undefined;
      mountedEl = null;
    };
  });

  return {
    instance,
    items: () => {
      resolved();
      tick();
      return instance.getVirtualItems();
    },
    totalSize: () => {
      resolved();
      tick();
      return instance.getTotalSize();
    },
    scrollToIndex: (index, opts) => untrack(() => instance.scrollToIndex(index, opts)),
    scrollToOffset: (offset, opts) => untrack(() => instance.scrollToOffset(offset, opts)),
    measureElement: (node) => untrack(() => instance.measureElement(node)),
  };
}

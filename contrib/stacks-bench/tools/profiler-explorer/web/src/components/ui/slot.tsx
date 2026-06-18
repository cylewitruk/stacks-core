import * as React from "react";

/**
 * Minimal Slot implementation (replaces @radix-ui/react-slot).
 * Merges its props into the single React element child,
 * allowing polymorphic rendering via the `asChild` pattern.
 */
const Slot = React.forwardRef<HTMLElement, React.PropsWithChildren<React.HTMLAttributes<HTMLElement>>>(({ children, ...slotProps }, forwardedRef) => {
  const child = React.Children.only(children);
  if (!React.isValidElement(child)) return null;

  // Merge refs
  const childRef = (child as React.ReactElement & { ref?: React.Ref<unknown> }).ref;
  const ref = mergeRefs(forwardedRef, childRef);

  // Merge classNames
  const mergedClassName = [slotProps.className, (child.props as Record<string, unknown>).className]
    .filter(Boolean)
    .join(" ") || undefined;

  // Merge styles
  const mergedStyle =
    slotProps.style || (child.props as Record<string, unknown>).style
      ? { ...slotProps.style, ...(child.props as React.CSSProperties) }
      : undefined;

  return React.cloneElement(child, {
    ...slotProps,
    ...(child.props as Record<string, unknown>),
    ref,
    ...(mergedClassName !== undefined && { className: mergedClassName }),
    ...(mergedStyle !== undefined && { style: mergedStyle }),
  } as React.Attributes & Record<string, unknown>);
});
Slot.displayName = "Slot";

function mergeRefs(...refs: (React.Ref<unknown> | undefined | null)[]) {
  return (value: unknown) => {
    for (const ref of refs) {
      if (typeof ref === "function") {
        ref(value);
      } else if (ref != null) {
        (ref as React.MutableRefObject<unknown>).current = value;
      }
    }
  };
}

export { Slot };

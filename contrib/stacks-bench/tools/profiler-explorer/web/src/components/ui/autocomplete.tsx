import * as React from "react";
import { Autocomplete as AutocompletePrimitive } from "@base-ui/react";
import { ChevronDownIcon, XIcon } from "lucide-react";
import { cn } from "@/lib/utils.ts";
import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupInput,
} from "@/components/ui/input-group";

const Autocomplete = AutocompletePrimitive.Root;

function AutocompleteInput({
  className,
  children,
  disabled = false,
  showTrigger = false,
  showClear = false,
  ...props
}: React.ComponentProps<typeof AutocompletePrimitive.Input> & {
  className?: string;
  children?: React.ReactNode;
  showTrigger?: boolean;
  showClear?: boolean;
  disabled?: boolean;
}) {
  return (
    <InputGroup className={cn("w-auto", className)}>
      <AutocompletePrimitive.Input
        render={<InputGroupInput disabled={disabled} />}
        {...props}
      />
      {(showTrigger || showClear) && (
        <InputGroupAddon align="inline-end">
          {showTrigger && (
            <InputGroupButton
              size="icon-xs"
              variant="ghost"
              asChild
              data-slot="input-group-button"
              className="group-has-data-[slot=autocomplete-clear]/input-group:hidden data-pressed:bg-transparent"
              disabled={disabled}
            >
              <AutocompletePrimitive.Trigger data-slot="autocomplete-trigger">
                <ChevronDownIcon className="text-muted-foreground pointer-events-none size-4" />
              </AutocompletePrimitive.Trigger>
            </InputGroupButton>
          )}
          {showClear && (
            <AutocompletePrimitive.Clear
              data-slot="autocomplete-clear"
              render={<InputGroupButton variant="ghost" size="icon-xs" />}
              className={cn(className)}
              disabled={disabled}
            >
              <XIcon className="pointer-events-none" />
            </AutocompletePrimitive.Clear>
          )}
        </InputGroupAddon>
      )}
      {children}
    </InputGroup>
  );
}

type Side = "top" | "right" | "bottom" | "left";
type Align = "start" | "center" | "end";

function AutocompleteContent({
  className,
  side = "bottom",
  sideOffset = 6,
  align = "start",
  alignOffset = 0,
  zIndex = "z-50",
  ...props
}: React.ComponentProps<typeof AutocompletePrimitive.Popup> & {
  className?: string;
  side?: Side;
  sideOffset?: number;
  align?: Align;
  alignOffset?: number;
  zIndex?: string;
}) {
  return (
    <AutocompletePrimitive.Portal>
      <AutocompletePrimitive.Positioner
        side={side}
        sideOffset={sideOffset}
        align={align}
        alignOffset={alignOffset}
        className={`isolate ${zIndex}`}
      >
        <AutocompletePrimitive.Popup
          data-slot="autocomplete-content"
          className={cn(
            "bg-popover text-popover-foreground data-open:animate-in data-closed:animate-out data-closed:fade-out-0 data-open:fade-in-0 data-closed:zoom-out-95 data-open:zoom-in-95 data-[side=bottom]:slide-in-from-top-2 data-[side=left]:slide-in-from-right-2 data-[side=right]:slide-in-from-left-2 data-[side=top]:slide-in-from-bottom-2 ring-foreground/10 relative max-h-96 w-(--anchor-width) max-w-(--available-width) min-w-[calc(var(--anchor-width)+--spacing(7))] origin-(--transform-origin) overflow-hidden rounded-md shadow-md ring-1 duration-100",
            className
          )}
          {...props}
        />
      </AutocompletePrimitive.Positioner>
    </AutocompletePrimitive.Portal>
  );
}

function AutocompleteList({
  className,
  ...props
}: React.ComponentProps<typeof AutocompletePrimitive.List> & {
  className?: string;
}) {
  return (
    <AutocompletePrimitive.List
      data-slot="autocomplete-list"
      className={cn(
        "max-h-[min(calc(--spacing(96)---spacing(1)),calc(var(--available-height)---spacing(1)))] scroll-py-1 overflow-y-auto p-1 data-empty:p-0",
        className
      )}
      {...props}
    />
  );
}

function AutocompleteItem({
  className,
  children,
  ...props
}: React.ComponentProps<typeof AutocompletePrimitive.Item> & {
  className?: string;
}) {
  return (
    <AutocompletePrimitive.Item
      data-slot="autocomplete-item"
      className={cn(
        "data-highlighted:bg-accent data-highlighted:text-accent-foreground relative flex w-full cursor-default items-center gap-2 rounded-sm py-1.5 px-2 text-sm outline-hidden select-none data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
        className
      )}
      {...props}
    >
      {children}
    </AutocompletePrimitive.Item>
  );
}

function AutocompleteEmpty({
  className,
  ...props
}: React.ComponentProps<typeof AutocompletePrimitive.Empty> & {
  className?: string;
}) {
  return (
    <AutocompletePrimitive.Empty
      data-slot="autocomplete-empty"
      className={cn(
        "text-muted-foreground py-2 text-center text-sm",
        className
      )}
      {...props}
    />
  );
}

function AutocompleteStatus({
  className,
  ...props
}: React.ComponentProps<typeof AutocompletePrimitive.Status> & {
  className?: string;
}) {
  return (
    <AutocompletePrimitive.Status
      data-slot="autocomplete-status"
      className={cn("text-muted-foreground px-2 py-1.5 text-xs", className)}
      {...props}
    />
  );
}

export {
  Autocomplete,
  AutocompleteInput,
  AutocompleteContent,
  AutocompleteList,
  AutocompleteItem,
  AutocompleteEmpty,
  AutocompleteStatus,
};

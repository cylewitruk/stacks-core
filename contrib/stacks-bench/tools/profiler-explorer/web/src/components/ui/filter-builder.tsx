import React, {
  Fragment,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { ChevronDown, Loader2, Plus, X } from "lucide-react";
import { Button } from "./button";
import { Input } from "./input";
import {
  Autocomplete,
  AutocompleteContent,
  AutocompleteInput,
  AutocompleteItem,
  AutocompleteList,
} from "./autocomplete";
import {
  Combobox,
  ComboboxCollection,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxGroup,
  ComboboxInput,
  ComboboxItem,
  ComboboxLabel,
  ComboboxList,
  ComboboxSeparator,
} from "./combobox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "./dropdown-menu";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "./tooltip";
import { Popover, PopoverTrigger, PopoverContent } from "./popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
} from "./select";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface OperatorDef {
  id: string;
  label: string;
}

export interface RichOption {
  value: string;
  label: string;
  description?: string;
}

export interface FilterFieldDef {
  id: string;
  label: string;
  type?: "text" | "number" | "enum";
  operators?: OperatorDef[];
  enumValues?: string[];
  modifier?: string;
  /** Rich options for searchable checkbox combobox (used with is/isNot). */
  richOptions?: RichOption[];
  /** Map a stored value back to a display label for chips. */
  chipLabel?: (value: string) => string;
  /** Optional group label for categorizing fields in the selector dropdown. */
  group?: string;
}

export interface FilterRule {
  id: string;
  field: string;
  operator: string;
  value?: string;
  values?: string[];
  modifier?: string;
  rules?: undefined;
}

export interface FilterGroupValue {
  id?: string;
  glue: "and" | "or";
  rules: (FilterRule | FilterGroupValue)[];
}

export type FilterOptionsCallback = (fieldId: string, query: string, signal: AbortSignal) => Promise<string[]>;

// ---------------------------------------------------------------------------
// Operator definitions
// ---------------------------------------------------------------------------

const TEXT_OPERATORS = [
  { id: "contains", label: "contains" },
  { id: "notContains", label: "does not contain" },
  { id: "equal", label: "equals" },
  { id: "notEqual", label: "does not equal" },
  { id: "beginsWith", label: "begins with" },
  { id: "endsWith", label: "ends with" },
];

const NUMBER_OPERATORS = [
  { id: "equal", label: "=" },
  { id: "notEqual", label: "≠" },
  { id: "greater", label: ">" },
  { id: "greaterOrEqual", label: "≥" },
  { id: "less", label: "<" },
  { id: "lessOrEqual", label: "≤" },
];

/** Operators that support the multi-select chip list (textual IN) */
const MULTI_VALUE_OPS = new Set(["equal", "contains", "beginsWith", "endsWith"]);

/** Operators for enum (checkbox-list) fields. */
const ENUM_OPERATORS = [
  { id: "is", label: "is" },
  { id: "isNot", label: "is not" },
];

function getOperators(field: FilterFieldDef | undefined | null): OperatorDef[] {
  if (field?.operators) return field.operators;
  if (field?.type === "enum") return ENUM_OPERATORS;
  return field?.type === "number" ? NUMBER_OPERATORS : TEXT_OPERATORS;
}

function getOperatorLabel(field: FilterFieldDef | undefined | null, opId: string): string {
  return getOperators(field).find((o: OperatorDef) => o.id === opId)?.label || opId;
}

function getFieldLabel(fields: FilterFieldDef[], fieldId: string): string {
  const f = fields.find((f: FilterFieldDef) => f.id === fieldId);
  if (!f) return fieldId;
  return f.group ? `${f.group}: ${f.label}` : f.label;
}

/** Whether a field supports multi-value (chip) selection. */
function supportsMultiValue(field: FilterFieldDef | undefined | null, operator: string): boolean {
  if (field?.type === "enum") return true;
  if ((operator === "is" || operator === "isNot") && field?.richOptions) return true;
  return field?.type !== "number" && MULTI_VALUE_OPS.has(operator);
}

/** Whether to show the rich searchable checkbox list (richOptions + is/isNot or enumValues + is/isNot on text fields). */
function useRichCombo(field: FilterFieldDef | undefined | null, operator: string): boolean {
  if (operator !== "is" && operator !== "isNot") return false;
  if (field?.richOptions?.length) return true;
  // Text field with enumValues + is/isNot → show checkbox list
  if (field?.type === "text" && field?.enumValues?.length) return true;
  return false;
}
// ---------------------------------------------------------------------------
// Stable unique IDs
// ---------------------------------------------------------------------------

let _nextId = 1;
const uid = () => `_fr${_nextId++}`;

// ---------------------------------------------------------------------------
// AutocompleteValueInput – text input with async suggestion list using
// the official @base-ui/react Autocomplete component.
// Re-mount via `key` prop when the field changes to clear stale results.
// ---------------------------------------------------------------------------

interface AutocompleteValueInputProps {
  value: string;
  onChange: (v: string) => void;
  onCommit?: (v: string) => void;
  fieldId: string;
  fieldType?: string;
  options?: FilterOptionsCallback;
  placeholder?: string;
}

function AutocompleteValueInput({
  value,
  onChange,
  onCommit,
  fieldId,
  fieldType,
  options,
  placeholder,
}: AutocompleteValueInputProps) {
  const [items, setItems] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  // Suppress next fetch cycle after an item is selected
  const suppressRef = useRef(false);

  // Debounced fetch whenever the input value changes
  const fetchItems = useCallback(
    (query: string) => {
      if (!options || !fieldId || fieldType === "number") return;
      if (!query || query.length === 0) {
        setItems([]);
        return;
      }
      if (suppressRef.current) {
        suppressRef.current = false;
        return;
      }

      abortRef.current?.abort();
      const controller = new AbortController();
      abortRef.current = controller;

      setLoading(true);
      const timer = setTimeout(async () => {
        try {
          const results = await options(fieldId, query, controller.signal);
          if (!controller.signal.aborted) {
            setItems(results || []);
          }
        } catch {
          if (!controller.signal.aborted) setItems([]);
        } finally {
          if (!controller.signal.aborted) setLoading(false);
        }
      }, 150);

      // Cleanup on next call
      return () => {
        clearTimeout(timer);
        controller.abort();
      };
    },
    [fieldId, options, fieldType]
  );

  // Memoize items for Autocomplete (must be referentially stable when unchanged)
  const stableItems = useMemo(() => items, [items]);

  // Handle selecting an item from the autocomplete dropdown
  const handleItemSelect = useCallback(
    (item: string) => {
      suppressRef.current = true;
      onChange(String(item));
      onCommit?.(String(item));
      // Refocus after the autocomplete closes
      setTimeout(() => inputRef.current?.focus(), 0);
    },
    [onChange, onCommit],
  );

  return (
    <Autocomplete
      items={stableItems}
      filter={null}
    >
      <AutocompleteInput
        ref={inputRef}
        placeholder={placeholder || "Enter value…"}
        autoFocus
        defaultValue={value}
        className="h-9 flex-1 min-w-0"
        onInput={(e: React.FormEvent<HTMLInputElement>) => {
          const v = (e.target as HTMLInputElement).value;
          onChange(v);
          fetchItems(v);
        }}
        onKeyDown={(e: React.KeyboardEvent<HTMLInputElement>) => {
          if (e.key === "Enter") {
            const curValue = (e.target as HTMLInputElement).value.trim();
            if (curValue) {
              // Let the Autocomplete handle Enter when an item is highlighted;
              // only commit raw text when nothing is highlighted.
              // We use a 0ms timeout so the Autocomplete's own Enter handler fires first.
              setTimeout(() => {
                if (inputRef.current && inputRef.current.value.trim() === curValue) {
                  suppressRef.current = true;
                  onCommit?.(curValue);
                }
              }, 0);
            }
          }
        }}
      />
      {loading && (
        <div className="pointer-events-none absolute right-2 top-1/2 -translate-y-1/2">
          <Loader2 className="text-muted-foreground h-4 w-4 animate-spin" />
        </div>
      )}
      <AutocompleteContent zIndex="z-[210]">
        <AutocompleteList>
          {(item: string) => (
            <AutocompleteItem key={item} value={item} onClick={() => handleItemSelect(item)}>
              {item}
            </AutocompleteItem>
          )}
        </AutocompleteList>
      </AutocompleteContent>
    </Autocomplete>
  );
}

// ---------------------------------------------------------------------------
// CheckboxList – checkbox-based value selector for enum fields
// ---------------------------------------------------------------------------

function CheckboxList({ enumValues, selected, onChange }: { enumValues: string[]; selected: string[]; onChange: (updater: (prev: string[]) => string[]) => void }) {
  const toggle = useCallback(
    (val: string) => {
      onChange((prev: string[]) =>
        prev.includes(val) ? prev.filter((v: string) => v !== val) : [...prev, val]
      );
    },
    [onChange]
  );

  if (!enumValues?.length) {
    return <div className="px-2 py-2 text-[0.8125rem] italic text-muted-foreground">No options available</div>;
  }

  return (
    <div className="flex max-h-[200px] flex-col gap-0.5 overflow-y-auto py-1">
      {enumValues.map((item: string) => {
        const checked = selected.includes(item);
        return (
          <label key={item} className="flex cursor-pointer items-center gap-2 rounded px-2 py-1 text-[0.8125rem] transition-colors hover:bg-accent">
            <input
              type="checkbox"
              checked={checked}
              onChange={() => toggle(item)}
              className="h-3.5 w-3.5 flex-shrink-0 cursor-pointer accent-primary"
            />
            <span className="text-foreground select-none">{item}</span>
          </label>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------
// SearchableCheckboxList – searchable combobox with rich two-line items
// ---------------------------------------------------------------------------

function SearchableCheckboxList({
  options,
  selected,
  onChange,
}: {
  options: RichOption[];
  selected: string[];
  onChange: (updater: (prev: string[]) => string[]) => void;
}) {
  const [query, setQuery] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const filtered = query
    ? options.filter((o) => {
        const q = query.toLowerCase();
        return (
          o.label.toLowerCase().includes(q) ||
          (o.description?.toLowerCase().includes(q) ?? false)
        );
      })
    : options;

  const toggle = useCallback(
    (val: string) => {
      onChange((prev: string[]) =>
        prev.includes(val) ? prev.filter((v: string) => v !== val) : [...prev, val]
      );
    },
    [onChange]
  );

  if (!options?.length) {
    return <div className="px-2 py-2 text-[0.8125rem] italic text-muted-foreground">No options available</div>;
  }

  return (
    <div className="flex flex-col gap-1">
      <Input
        ref={inputRef}
        type="text"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        placeholder="Search…"
        className="text-[0.8125rem]"
        autoFocus
      />
      <div className="flex max-h-60 flex-col gap-0.5 overflow-y-auto py-1">
        {filtered.length === 0 && (
          <div className="px-2 py-2 text-[0.8125rem] italic text-muted-foreground">No matches</div>
        )}
        {filtered.map((opt) => {
          const checked = selected.includes(opt.value);
          return (
            <label key={opt.value} className="flex cursor-pointer items-start gap-2 rounded px-2 py-[5px] transition-colors hover:bg-accent">
              <input
                type="checkbox"
                checked={checked}
                onChange={() => toggle(opt.value)}
                className="mt-0.5 h-3.5 w-3.5 flex-shrink-0 cursor-pointer accent-primary"
              />
              <span className="flex min-w-0 flex-col gap-px">
                <span className="truncate text-[0.8125rem] font-semibold text-foreground select-none">{opt.label}</span>
                {opt.description && (
                  <span className="truncate text-[0.6875rem] text-muted-foreground select-none">{opt.description}</span>
                )}
              </span>
            </label>
          );
        })}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// FieldCombobox – searchable field selector using the official Combobox
// with grouped items (e.g. "Clarity" group).
// ---------------------------------------------------------------------------

interface FieldGroup {
  value: string;
  items: string[];
}

function FieldCombobox({
  fields,
  value,
  onValueChange,
}: {
  fields: FilterFieldDef[];
  value: string;
  onValueChange: (v: string) => void;
}) {
  // Build grouped items structure for the Combobox
  const groups = useMemo(() => {
    const ungrouped: string[] = [];
    const groupMap = new Map<string, string[]>();
    for (const f of fields) {
      if (f.group) {
        let arr = groupMap.get(f.group);
        if (!arr) { arr = []; groupMap.set(f.group, arr); }
        arr.push(f.id);
      } else {
        ungrouped.push(f.id);
      }
    }
    const result: FieldGroup[] = [];
    if (ungrouped.length) result.push({ value: "", items: ungrouped });
    for (const [groupLabel, ids] of groupMap) {
      result.push({ value: groupLabel, items: ids });
    }
    return result;
  }, [fields]);

  // Map field ID → label for display and filtering
  const labelMap = useMemo(() => {
    const map = new Map<string, string>();
    for (const f of fields) map.set(f.id, f.label);
    return map;
  }, [fields]);

  return (
    <Combobox
      value={value}
      onValueChange={(v) => { if (v != null) onValueChange(String(v)); }}
      items={groups}
      itemToStringValue={(id) => labelMap.get(String(id)) ?? String(id)}
      itemToStringLabel={(id) => labelMap.get(String(id)) ?? String(id)}
    >
      <ComboboxInput
        placeholder="Select field…"
        className="fb-editor-field-select h-9"
      />
      <ComboboxContent zIndex="z-[210]">
        <ComboboxEmpty>No fields found.</ComboboxEmpty>
        <ComboboxList>
          {(group: FieldGroup, index: number) => (
            <ComboboxGroup key={group.value || "__ungrouped__"} items={group.items}>
              {group.value && <ComboboxLabel>{group.value}</ComboboxLabel>}
              <ComboboxCollection>
                {(fieldId: string) => (
                  <ComboboxItem key={fieldId} value={fieldId}>
                    {labelMap.get(fieldId) ?? fieldId}
                  </ComboboxItem>
                )}
              </ComboboxCollection>
              {index < groups.length - 1 && <ComboboxSeparator />}
            </ComboboxGroup>
          )}
        </ComboboxList>
      </ComboboxContent>
    </Combobox>
  );
}

// ---------------------------------------------------------------------------
// FilterEditor – the popover body for adding / editing a single rule
// New layout:  Row 1: [Field selector]          [Condition type]
//              Row 2: [Value input]             [Modifier (opt)]
//              Row 3: (multi-select chips for textual IN)
// ---------------------------------------------------------------------------

/** Duration unit options for fields that have `modifier: "duration"`. */
const DURATION_UNITS = [
  { id: "ms", label: "ms" },
  { id: "us", label: "μs" },
  { id: "s",  label: "s"  },
];

interface FilterEditorProps {
  rule: FilterRule | null;
  fields: FilterFieldDef[];
  options?: FilterOptionsCallback;
  onApply: (data: Omit<FilterRule, "id">) => void;
  onCancel: () => void;
}

function FilterEditor({ rule, fields, options, onApply, onCancel }: FilterEditorProps) {
  const [field, setField] = useState(rule?.field || fields[0]?.id || "");
  const [operator, setOperator] = useState(rule?.operator || "");
  // If reopening a multi-value rule, start the input empty — chips already show the values
  const [value, setValue] = useState(rule?.values?.length ? "" : (rule?.value ?? ""));
  const [values, setValues] = useState<string[]>(rule?.values ?? []);
  const [modifier, setModifier] = useState(rule?.modifier ?? "ms");
  // Key to force re-mount AutocompleteInput when field changes (clears stale results)
  const [acKey, setAcKey] = useState(0);
  // Track first mount so we don't wipe restored values on initial render
  const mountedRef = useRef(false);

  const selectedField = fields.find((f: FilterFieldDef) => f.id === field);
  const operators = getOperators(selectedField);
  const isNumeric = selectedField?.type === "number";
  const isEnum = selectedField?.type === "enum";
  const hasDurationModifier = selectedField?.modifier === "duration";
  const isMulti = supportsMultiValue(selectedField, operator);
  const isRichCombo = useRichCombo(selectedField, operator);

  // Reset operator + value + chips + autocomplete when field changes
  // (skip on initial mount so we don't wipe restored values from rule prop)
  useEffect(() => {
    if (!mountedRef.current) {
      mountedRef.current = true;
      return;
    }
    const ops = getOperators(fields.find((f: FilterFieldDef) => f.id === field));
    if (ops.length && !ops.find((o: OperatorDef) => o.id === operator)) {
      setOperator(ops[0].id);
    }
    setValue("");
    setValues([]);
    setAcKey((k) => k + 1);
  }, [field]); // eslint-disable-line react-hooks/exhaustive-deps

  // Set initial operator
  useEffect(() => {
    if (!operator && operators.length) setOperator(operators[0].id);
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // When operator changes and goes from multi→non-multi, collapse chips to single value
  useEffect(() => {
    if (!supportsMultiValue(selectedField, operator) && values.length > 0) {
      setValue(values[0] || "");
      setValues([]);
    }
  }, [operator]); // eslint-disable-line react-hooks/exhaustive-deps

  /** Add a value to the multi-select chip list. */
  const addChip = useCallback((v: string) => {
    const trimmed = String(v).trim();
    if (!trimmed) return;
    setValues((prev: string[]) => prev.includes(trimmed) ? prev : [...prev, trimmed]);
    setValue("");
    setAcKey((k) => k + 1);
  }, []);

  const removeChip = useCallback((v: string) => {
    setValues((prev: string[]) => prev.filter((x: string) => x !== v));
  }, []);

  const apply = () => {
    if (!field || !operator) return;
    // Enum and rich-combo fields require at least one selection
    if ((isEnum || isRichCombo) && values.length === 0) return;
    const result: Record<string, unknown> = { field, operator };
    if (isMulti && values.length > 0) {
      result.values = values;
      // For rich combobox, show labels instead of raw values in the chip summary
      if (isRichCombo && selectedField?.richOptions) {
        const labelMap = new Map(selectedField.richOptions.map((o) => [o.value, o.label]));
        result.value = values.map((v) => labelMap.get(v) ?? v).join(", ");
      } else {
        result.value = values.join(", ");
      }
    } else {
      result.value = value;
    }
    if (hasDurationModifier) {
      result.modifier = modifier;
    }
    onApply(result as Omit<FilterRule, "id">);
  };

  return (
    <div
      className="fb-editor"
      onKeyDown={(e) => {
        if (e.key === "Escape") onCancel();
      }}
    >
      {/* Row 1: Field selector + Operator selector */}
      <div className="fb-editor-row1">
        <FieldCombobox
          fields={fields}
          value={field}
          onValueChange={(v) => { if (v !== null) setField(v); }}
        />

        <Select value={operator} onValueChange={(v) => { if (v !== null) setOperator(v); }}>
          <SelectTrigger className="fb-editor-op-select">
            <span className="truncate">
              {getOperatorLabel(selectedField, operator)}
            </span>
          </SelectTrigger>
          <SelectContent>
            {operators.map((o: OperatorDef) => (
              <SelectItem key={o.id} value={o.id}>
                {o.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {/* Row 2: Value input (enum checkbox / rich combobox / numeric / text) */}
      {isRichCombo ? (
        <SearchableCheckboxList
          options={
            selectedField?.richOptions ??
            (selectedField?.enumValues ?? []).map((v) => ({ value: v, label: v }))
          }
          selected={values}
          onChange={setValues}
        />
      ) : isEnum ? (
        <CheckboxList
          enumValues={selectedField?.enumValues ?? []}
          selected={values}
          onChange={setValues}
        />
      ) : (
      <div className="fb-editor-row2">
        {isNumeric ? (
          <Input
            type="number"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); apply(); } }}
            placeholder="Enter value…"
            className="w-40 flex-shrink-0"
            autoFocus
          />
        ) : (
          <AutocompleteValueInput
            key={acKey}
            value={value}
            onChange={setValue}
            onCommit={isMulti ? addChip : undefined}
            fieldId={field}
            fieldType={selectedField?.type}
            options={options}
            placeholder={isMulti && values.length ? "Add another…" : "Enter value…"}
          />
        )}

        {hasDurationModifier && (
          <Select value={modifier} onValueChange={(v) => { if (v !== null) setModifier(v); }}>
            <SelectTrigger className="fb-editor-modifier-select">
              <span className="truncate">
                {DURATION_UNITS.find((u) => u.id === modifier)?.label || modifier}
              </span>
            </SelectTrigger>
            <SelectContent>
              {DURATION_UNITS.map((u) => (
                <SelectItem key={u.id} value={u.id}>
                  {u.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
      </div>
      )}

      {/* Row 3: Multi-select chips (text fields only, not enum/richCombo) */}
      {!isEnum && !isRichCombo && isMulti && values.length > 0 && (
        <div className="fb-editor-chips">
          {values.map((v: string) => (
            <span key={v} className="inline-flex h-[26px] max-w-[260px] items-center gap-1 rounded-md border border-border bg-muted px-1 pl-2 text-xs leading-none text-foreground">
              <span className="truncate" title={v}>{v}</span>
              <button
                type="button"
                className="inline-flex h-[18px] w-[18px] flex-shrink-0 cursor-pointer items-center justify-center rounded-sm border-none bg-transparent p-0 text-muted-foreground transition-colors hover:bg-destructive hover:text-destructive-foreground"
                onClick={() => removeChip(v)}
                aria-label={`Remove ${v}`}
              >
                <X size={12} />
              </button>
            </span>
          ))}
        </div>
      )}

      {/* Actions */}
      <div className="fb-editor-actions">
        <Button variant="ghost" size="sm" onClick={onCancel}>
          Cancel
        </Button>
        <Button size="sm" onClick={apply}>
          Apply
        </Button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// GlueToggle – clickable and / or pill
// ---------------------------------------------------------------------------

function GlueToggle({ glue, onClick }: { glue: string; onClick: () => void }) {
  return (
    <button
      type="button"
      className={`fb-glue${glue === "or" ? " fb-glue-or" : ""}`}
      onClick={onClick}
    >
      {glue}
    </button>
  );
}

// ---------------------------------------------------------------------------
// FilterChip – displays one rule with an X button
// ---------------------------------------------------------------------------

function FilterChip({ rule, fields, onEdit, onDelete }: { rule: FilterRule; fields: FilterFieldDef[]; onEdit: () => void; onDelete: () => void }) {
  const field = fields.find((f: FilterFieldDef) => f.id === rule.field);
  const opLabel = getOperatorLabel(field, rule.operator);
  let displayValue: string;
  if (rule.values?.length) {
    if (field?.chipLabel) {
      // Show mapped labels for up to 2 values, then "N values" for the rest
      const labels = rule.values.map((v) => field.chipLabel!(v));
      displayValue = labels.length <= 2 ? labels.join(", ") : `${labels.length} values`;
    } else {
      displayValue = `${rule.values.length} value${rule.values.length > 1 ? "s" : ""}`;
    }
  } else {
    displayValue = String(rule.value);
  }
  const isDuration = field?.modifier === "duration";
  const displaySuffix = isDuration && rule.modifier && rule.modifier !== "ms"
    ? ` (${rule.modifier})`
    : "";
  return (
    <div className="fb-chip" role="button" tabIndex={0} onClick={onEdit}>
      <span className="fb-chip-field">{field?.label || rule.field}</span>
      <span className="fb-chip-op">{opLabel}</span>
      <span className="fb-chip-value">{displayValue}{displaySuffix}</span>
      <button
        type="button"
        className="fb-chip-x"
        tabIndex={-1}
        onClick={(e) => {
          e.stopPropagation();
          onDelete();
        }}
        aria-label="Remove filter"
      >
        <X size={14} />
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// GroupChip – displays a nested group as a bordered inline container
// ---------------------------------------------------------------------------

function GroupChip({ group, fields, options, onUpdate, onDelete }: { group: FilterGroupValue; fields: FilterFieldDef[]; options?: FilterOptionsCallback; onUpdate: (updated: FilterGroupValue) => void; onDelete: () => void }) {
  return (
    <div className="fb-group">
      <div className="fb-group-header">
        <button
          type="button"
          className="fb-group-x"
          onClick={onDelete}
          aria-label="Remove group"
        >
          <X size={12} />
        </button>
      </div>
      <FilterGroup
        value={group}
        fields={fields}
        onChange={onUpdate}
        options={options}
        depth={1}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// FilterGroup – renders one level of rules + glue toggles (recursive)
// ---------------------------------------------------------------------------

function FilterGroup({ value, fields, onChange, options, depth = 0 }: { value: FilterGroupValue; fields: FilterFieldDef[]; onChange: (v: FilterGroupValue) => void; options?: FilterOptionsCallback; depth?: number }) {
  const [editingId, setEditingId] = useState<string | null>(null);

  const rules = value?.rules || [];
  const glue = value?.glue || "and";

  const addRule = useCallback(
    (data: Omit<FilterRule, "id">) => {
      const rule = { id: uid(), ...data };
      onChange({ glue, rules: [...rules, rule] });
      setEditingId(null);
    },
    [glue, rules, onChange],
  );

  const updateRule = useCallback(
    (ruleId: string, data: Partial<FilterRule>) => {
      onChange({
        ...value,
        rules: rules.map((r: FilterRule | FilterGroupValue) => (r.id === ruleId ? { ...r, ...data } : r)),
      });
      setEditingId(null);
    },
    [value, rules, onChange],
  );

  const deleteRule = useCallback(
    (ruleId: string) => {
      onChange({ ...value, rules: rules.filter((r: FilterRule | FilterGroupValue) => r.id !== ruleId) });
      if (editingId === ruleId) setEditingId(null);
    },
    [value, rules, onChange, editingId],
  );

  const toggleGlue = useCallback(() => {
    onChange({ ...value, glue: glue === "and" ? "or" : "and" });
  }, [value, glue, onChange]);

  const addGroup = useCallback(() => {
    const group: FilterGroupValue = { id: uid(), glue: "or" as const, rules: [] };
    onChange({ glue, rules: [...rules, group] });
  }, [glue, rules, onChange]);

  const updateGroup = useCallback(
    (groupId: string, newGroup: Partial<FilterGroupValue>) => {
      onChange({
        ...value,
        rules: rules.map((r: FilterRule | FilterGroupValue) => (r.id === groupId ? { ...r, ...newGroup } : r)) as (FilterRule | FilterGroupValue)[],
      });
    },
    [value, rules, onChange],
  );

  const handleApply = useCallback(
    (data: Omit<FilterRule, "id">) => {
      if (editingId && editingId !== "__new__") {
        updateRule(editingId, data);
      } else {
        addRule(data);
      }
    },
    [editingId, addRule, updateRule],
  );

  const closeEditor = useCallback(() => setEditingId(null), []);

  return (
    <div className="fb-bar">
      {rules.map((rule: FilterRule | FilterGroupValue, index: number) => {
        const isGroup = Array.isArray((rule as FilterGroupValue).rules);
        return (
          <Fragment key={rule.id}>
            {index > 0 && <GlueToggle glue={glue} onClick={toggleGlue} />}

            {isGroup ? (
              <GroupChip
                group={rule as FilterGroupValue}
                fields={fields}
                options={options}
                onUpdate={(updated: FilterGroupValue) => updateGroup(rule.id!, updated)}
                onDelete={() => deleteRule(rule.id!)}
              />
            ) : (
              <Popover
                open={editingId === rule.id}
                onOpenChange={(open) => {
                  if (!open) setEditingId(null);
                }}
              >
                <PopoverTrigger asChild>
                  <span>
                    <FilterChip
                      rule={rule as FilterRule}
                      fields={fields}
                      onEdit={() => setEditingId(rule.id!)}
                      onDelete={() => deleteRule(rule.id!)}
                    />
                  </span>
                </PopoverTrigger>
                <PopoverContent
                  side="bottom"
                  align="start"
                  sideOffset={6}
                  className="fb-popover"
                >
                  <FilterEditor
                    rule={rule as FilterRule}
                    fields={fields}
                    options={options}
                    onApply={handleApply}
                    onCancel={closeEditor}
                  />
                </PopoverContent>
              </Popover>
            )}
          </Fragment>
        );
      })}

      {/* Split button: + Filter (primary) with dropdown for + Group */}
      <div className="fb-split-btn">
        <Popover
          open={editingId === "__new__"}
          onOpenChange={(open) => {
            if (open) setEditingId("__new__");
            else setEditingId(null);
          }}
        >
          <PopoverTrigger asChild>
            <Button variant="default" size="sm" className="fb-split-primary">
              <Plus size={14} className="mr-1" />
              Filter
            </Button>
          </PopoverTrigger>
          <PopoverContent
            side="bottom"
            align="start"
            sideOffset={6}
            className="fb-popover"
          >
            <FilterEditor
              rule={null}
              fields={fields}
              options={options}
              onApply={handleApply}
              onCancel={closeEditor}
            />
          </PopoverContent>
        </Popover>

        {depth === 0 && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="default" size="sm" className="fb-split-chevron" aria-label="More filter options">
                <ChevronDown size={14} />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent side="bottom" align="start">
              <DropdownMenuItem onClick={addGroup}>
                <Plus size={14} className="mr-2" />
                Add Group
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// FilterBuilder – the main exported component
// ---------------------------------------------------------------------------

/**
 * A fully custom, reusable filter-bar component with nested group support.
 *
 * @param {Object}   props
 * @param {Array}    props.fields   – field definitions `{ id, label, type, operators? }`
 * @param {Object}   props.value    – filter state `{ glue, rules: [rule|group] }`
 *        rule = `{ id, field, operator, value }`
 *        group = `{ id, glue, rules: [rule|group] }`
 * @param {Function} props.onChange – called with the updated filter-state object
 * @param {Function} [props.options] – async autocomplete `(fieldId, query, signal) => Promise<string[]>`
 * @param {boolean}  [props.filtersEnabled] – whether filters are active (true) or bypassed (false)
 * @param {Function} [props.onToggleEnabled] – toggle enabled/disabled
 * @param {Function} [props.onClear] – clear all filters
 */
export function FilterBuilder({
  fields,
  value,
  onChange,
  options,
  filtersEnabled,
  onToggleEnabled,
  onClear,
}: {
  fields: FilterFieldDef[];
  value: FilterGroupValue;
  onChange: (v: FilterGroupValue) => void;
  options?: FilterOptionsCallback;
  filtersEnabled?: boolean;
  onToggleEnabled?: () => void;
  onClear?: () => void;
}) {
  const hasRules = value?.rules?.length > 0;

  return (
    <div className={`fb-outer${hasRules && filtersEnabled === false ? " fb-disabled" : ""}`}>
      {/* Enable/disable toggle – only visible when there are filters */}
      {hasRules && onToggleEnabled && (
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              className={`fb-toggle${filtersEnabled === false ? " fb-toggle-off" : ""}`}
              onClick={onToggleEnabled}
              aria-label={filtersEnabled === false ? "Enable filters" : "Disable filters"}
            >
              <span className="fb-toggle-track">
                <span className="fb-toggle-thumb" />
              </span>
            </button>
          </TooltipTrigger>
          <TooltipContent side="bottom">
            {filtersEnabled === false ? "Enable filters" : "Disable filters"}
          </TooltipContent>
        </Tooltip>
      )}

      <FilterGroup
        value={value}
        fields={fields}
        onChange={onChange}
        options={options}
        depth={0}
      />

      {/* Clear filters button */}
      {hasRules && onClear && (
        <Tooltip>
          <TooltipTrigger asChild>
            <button
              type="button"
              className="fb-clear-btn"
              onClick={onClear}
              aria-label="Clear all filters"
            >
              <X size={14} />
            </button>
          </TooltipTrigger>
          <TooltipContent side="bottom">Clear all filters</TooltipContent>
        </Tooltip>
      )}
    </div>
  );
}

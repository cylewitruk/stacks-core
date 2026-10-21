import React, { useMemo, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Columns3, ChevronDown, ChevronRight, Check, Minus } from "lucide-react";
import { COLUMN_DEFS } from "../columnsConfig.ts";

/**
 * Build column groups structure for the selector.
 * Groups: Wall Time, Busy Time, Wait Time, Clarity
 * Each group has subgroups with Total/Avg toggles
 */
interface BuildGroupEntry {
  key: string;
  label: string;
  subgroups: Map<string, ColumnGroupSubgroup>;
  columnKeys: string[];
}

function buildGroups(): { groups: ColumnGroup[]; standalone: { key: string; label: string }[] } {
  const groups: BuildGroupEntry[] = [];
  const groupMap = new Map<string, BuildGroupEntry>();
  const standalone: { key: string; label: string }[] = [];

  for (const col of COLUMN_DEFS) {
    if (col.alwaysHidden || col.selectable === false || col.alwaysVisible) continue;

    if (!col.level1) {
      standalone.push({ key: col.key, label: col.label });
    } else {
      if (!groupMap.has(col.level1)) {
        const group: BuildGroupEntry = {
          key: col.level1,
          label: col.level1,
          subgroups: new Map(),
          columnKeys: [],
        };
        groupMap.set(col.level1, group);
        groups.push(group);
      }

      const group = groupMap.get(col.level1)!;
      group.columnKeys.push(col.key);

      if (col.level2) {
        if (!group.subgroups.has(col.level2)) {
          group.subgroups.set(col.level2, {
            key: `${col.level1}::${col.level2}`,
            label: col.level2,
            columns: [],
          });
        }
        const subgroup = group.subgroups.get(col.level2)!;
        subgroup.columns.push({
          key: col.key,
          label: col.level3 || col.label,
          isTotal: col.level3 === "Total",
          isAvg: col.level3 === "Avg." || col.level3 === "Avg",
        });
      }
    }
  }

  const result: ColumnGroup[] = groups.map((g) => ({
    key: g.key,
    label: g.label,
    subgroups: Array.from(g.subgroups.values()),
    columnKeys: g.columnKeys,
  }));

  return { groups: result, standalone };
}

interface TriStateCheckboxProps {
  state: "all" | "some" | "none";
  onChange: (e: React.MouseEvent) => void;
  className?: string;
}

function TriStateCheckbox({ state, onChange, className = "" }: TriStateCheckboxProps) {
  // state: "all" | "some" | "none"
  return (
    <button
      type="button"
      onClick={onChange}
      className={`w-4 h-4 rounded border flex items-center justify-center transition-colors ${
        state === "all"
          ? "bg-primary border-primary text-primary-foreground"
          : state === "some"
            ? "bg-primary/50 border-primary text-primary-foreground"
            : "bg-background border-muted-foreground/40 hover:border-muted-foreground"
      } ${className}`}
    >
      {state === "all" && <Check className="w-3 h-3" />}
      {state === "some" && <Minus className="w-3 h-3" />}
    </button>
  );
}

interface ToggleButtonProps {
  checked: boolean;
  onChange: () => void;
  children: React.ReactNode;
  className?: string;
}

function ToggleButton({ checked, onChange, children, className = "" }: ToggleButtonProps) {
  return (
    <button
      type="button"
      onClick={onChange}
      className={`px-2 py-0.5 text-xs rounded border transition-colors ${
        checked
          ? "bg-primary border-primary text-primary-foreground"
          : "bg-background border-muted-foreground/30 text-muted-foreground hover:border-muted-foreground/50"
      } ${className}`}
    >
      {children}
    </button>
  );
}

interface ColumnGroupSubgroupColumn {
  key: string;
  label: string;
  isTotal: boolean;
  isAvg: boolean;
}

interface ColumnGroupSubgroup {
  key: string;
  label: string;
  columns: ColumnGroupSubgroupColumn[];
}

interface ColumnGroup {
  key: string;
  label: string;
  subgroups: ColumnGroupSubgroup[];
  columnKeys: string[];
}

interface GroupSectionProps {
  group: ColumnGroup;
  selected: string[];
  onToggle: (key: string) => void;
  onToggleGroup: (keys: string[], enable: boolean) => void;
  defaultOpen?: boolean;
}

function GroupSection({ group, selected, onToggle, onToggleGroup, defaultOpen = false }: GroupSectionProps) {
  const [isOpen, setIsOpen] = useState(defaultOpen);

  const selectedCount = group.columnKeys.filter((k) => selected.includes(k)).length;
  const totalCount = group.columnKeys.length;
  const state = selectedCount === 0 ? "none" : selectedCount === totalCount ? "all" : "some";

  const handleGroupToggle = () => {
    onToggleGroup(group.columnKeys, state !== "all");
  };

  return (
    <div className="border-b border-border/50 last:border-b-0">
      {/* Section header */}
      <div
        className="flex items-center gap-2 px-3 py-2 cursor-pointer hover:bg-muted/30"
        onClick={() => setIsOpen(!isOpen)}
      >
        <button type="button" className="text-muted-foreground">
          {isOpen ? (
            <ChevronDown className="w-4 h-4" />
          ) : (
            <ChevronRight className="w-4 h-4" />
          )}
        </button>
        <TriStateCheckbox
          state={state}
          onChange={(e) => {
            e.stopPropagation();
            handleGroupToggle();
          }}
        />
        <span className="font-medium text-sm flex-1">{group.label}</span>
        <span className="text-xs text-muted-foreground">
          {selectedCount}/{totalCount}
        </span>
      </div>

      {/* Section content */}
      {isOpen && (
        <div className="px-3 pb-2 pl-9 space-y-1.5">
          {/* Quick actions */}
          <div className="flex gap-2 text-xs mb-2">
            <button
              type="button"
              className="text-muted-foreground hover:text-foreground"
              onClick={() => onToggleGroup(group.columnKeys, true)}
            >
              All
            </button>
            <span className="text-muted-foreground/50">·</span>
            <button
              type="button"
              className="text-muted-foreground hover:text-foreground"
              onClick={() => onToggleGroup(group.columnKeys, false)}
            >
              None
            </button>
          </div>

          {/* Subgroup rows */}
          {group.subgroups.map((subgroup) => (
            <div key={subgroup.key} className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground w-24 shrink-0">
                {subgroup.label}:
              </span>
              <div className="flex gap-1.5">
                {subgroup.columns.map((col) => (
                  <ToggleButton
                    key={col.key}
                    checked={selected.includes(col.key)}
                    onChange={() => onToggle(col.key)}
                  >
                    {col.label}
                  </ToggleButton>
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

interface ColumnSelectorProps {
  selected: string[];
  onChange: (key: string) => void;
  onToggleGroup: (keys: string[], enable: boolean) => void;
}

export default function ColumnSelector({ selected, onChange, onToggleGroup }: ColumnSelectorProps) {
  const { groups, standalone } = useMemo(() => buildGroups(), []);
  const [open, setOpen] = useState(false);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant="outline" size="sm" className="gap-2">
          <Columns3 className="h-4 w-4" />
          Columns
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-80 p-0" sideOffset={4}>
        <div className="px-3 py-2 border-b border-border">
          <h4 className="font-medium text-sm">Visible Columns</h4>
        </div>

        <div className="max-h-[60vh] overflow-auto">
          {/* Standalone columns */}
          {standalone.length > 0 && (
            <div className="px-3 py-2 border-b border-border/50 space-y-1">
              {standalone.map((col) => (
                <label
                  key={col.key}
                  className="flex items-center gap-2 cursor-pointer py-0.5"
                >
                  <input
                    type="checkbox"
                    checked={selected.includes(col.key)}
                    onChange={() => onChange(col.key)}
                    className="rounded border-muted-foreground/40"
                  />
                  <span className="text-sm">{col.label}</span>
                </label>
              ))}
            </div>
          )}

          {/* Grouped sections */}
          {groups.map((group, i) => (
            <GroupSection
              key={group.key}
              group={group}
              selected={selected}
              onToggle={onChange}
              onToggleGroup={onToggleGroup}
              defaultOpen={i === 0}
            />
          ))}
        </div>
      </PopoverContent>
    </Popover>
  );
}

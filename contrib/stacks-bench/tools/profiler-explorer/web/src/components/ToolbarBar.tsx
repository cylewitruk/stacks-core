import React from "react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Flame, Link2 } from "lucide-react";
import ColumnSelector from "./ColumnSelector";

interface HotPathDropdownProps {
  hotPathMode: string;
  setHotPathMode: (mode: string) => void;
}

function HotPathDropdown({ hotPathMode, setHotPathMode }: HotPathDropdownProps) {
  const isActive = hotPathMode !== "off";
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant={isActive ? "default" : "outline"}
          size="sm"
          className={`gap-2 ${isActive ? "bg-orange-600 hover:bg-orange-700 text-white" : ""}`}
        >
          <Flame className="h-4 w-4" />
          Hot Path
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        <DropdownMenuGroup>
          <DropdownMenuLabel>Hot Path Mode</DropdownMenuLabel>
          <DropdownMenuSeparator />
          <DropdownMenuCheckboxItem
            checked={hotPathMode === "off"}
            onCheckedChange={() => setHotPathMode("off")}
          >
            Off
          </DropdownMenuCheckboxItem>
          <DropdownMenuCheckboxItem
            checked={hotPathMode === "inclusive"}
            onCheckedChange={() => setHotPathMode("inclusive")}
          >
            Inclusive Time
          </DropdownMenuCheckboxItem>
          <DropdownMenuCheckboxItem
            checked={hotPathMode === "self"}
            onCheckedChange={() => setHotPathMode("self")}
          >
            Self Time
          </DropdownMenuCheckboxItem>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

interface ToolbarBarProps {
  selectedColumns: string[];
  toggleColumn: (key: string) => void;
  expandToDepth: (depth: number) => void;
  hotPathMode: string;
  setHotPathMode: (mode: string) => void;
  chainCompression: boolean;
  setChainCompression: (val: boolean) => void;
  collapseSiblings: () => void;
  activeId: string | number | null;
  focusId: string | number | null;
  clearFocus: () => void;
  summary: string | null;
  toggleColumnGroup: (keys: string[], enable: boolean) => void;
}

export default function ToolbarBar({
  selectedColumns,
  toggleColumn,
  expandToDepth,
  hotPathMode,
  setHotPathMode,
  chainCompression,
  setChainCompression,
  collapseSiblings,
  activeId,
  focusId,
  clearFocus,
  summary,
  toggleColumnGroup,
}: ToolbarBarProps) {
  return (
    <div className="app-toolbar">
      <div className="toolbar-left">
        <ColumnSelector
          selected={selectedColumns}
          onChange={toggleColumn}
          onToggleGroup={toggleColumnGroup}
        />
        <div className="toolbar-divider" />
        <div className="toolbar-group">
          <span className="toolbar-label">Depth:</span>
          {[2, 4, 6].map((depth) => (
            <Button
              key={depth}
              variant="outline"
              size="sm"
              onClick={() => expandToDepth(depth)}
              disabled={hotPathMode !== "off"}
            >
              {depth}
            </Button>
          ))}
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={collapseSiblings}
          disabled={!activeId}
        >
          Collapse siblings
        </Button>
        {focusId && (
          <Button
            variant="outline"
            size="sm"
            onClick={clearFocus}
            className="border-primary text-primary hover:bg-primary/10"
          >
            Clear focus
          </Button>
        )}
        <div className="toolbar-divider" />
        <HotPathDropdown
          hotPathMode={hotPathMode}
          setHotPathMode={setHotPathMode}
        />
        <Button
          variant={chainCompression ? "default" : "outline"}
          size="sm"
          className={`gap-2 ${chainCompression ? "bg-emerald-600 hover:bg-emerald-700 text-white" : ""}`}
          onClick={() => setChainCompression(!chainCompression)}
        >
          <Link2 className="h-4 w-4" />
          Compress Chains
        </Button>
      </div>
      <div className="toolbar-right">
        {summary && <span className="toolbar-status">{summary}</span>}
      </div>
    </div>
  );
}

import React, { useCallback, useMemo, useState } from "react";
import { Grid, ContextMenu } from "@svar-ui/react-grid";
import { WillowDark } from "@svar-ui/react-core";
import { Loader2 } from "lucide-react";

interface TreeNode {
  id: string | number;
  data?: TreeNode[];
  _chain?: unknown[];
  [key: string]: unknown;
}

interface GridApi {
  exec: (command: string, params: Record<string, unknown>) => void;
  [key: string]: unknown;
}

interface ProfilerGridProps {
  data: TreeNode[];
  columns: Record<string, unknown>[];
  spanVizEnabled: boolean;
  spanVizStyle: string;
  spanVizColor: string;
  isLoading: boolean;
  isEmpty: boolean;
  rowStyle: (row: Record<string, unknown>) => string;
  columnStyle: (column: Record<string, unknown>) => string;
  onOpenRow: (ev: { id: string | number }) => void;
  onCloseRow: (ev: { id: string | number }) => void;
  onSelectRow?: (ev: { id?: string | number }) => void;
  onInit?: (api: GridApi) => void;
  onViewDetails?: (row: TreeNode) => void;
  onCollapseSiblings?: (id: string | number) => void;
  onFocus?: (id: string | number) => void;
  onClearFocus?: () => void;
  onExpandChain?: (id: string | number) => void;
  focusId: string | number | null;
}

export default function ProfilerGrid({
  data,
  columns,
  spanVizEnabled,
  spanVizStyle,
  spanVizColor,
  isLoading,
  isEmpty,
  rowStyle,
  columnStyle,
  onOpenRow,
  onCloseRow,
  onSelectRow,
  onInit,
  onViewDetails,
  onCollapseSiblings,
  onFocus,
  onClearFocus,
  onExpandChain,
  focusId,
}: ProfilerGridProps) {
  const [gridApi, setGridApi] = useState<GridApi | null>(null);
  const [selectedRowId, setSelectedRowId] = useState<string | number | null>(null);

  // Close the context menu when a row is selected (clicked inside grid)
  // We simulate a mousedown on the document body which triggers svar-ui's clickOutside handler
  const handleSelectRow = useCallback((ev: { id?: string | number }) => {
    // Dispatch a mousedown event on document to trigger context menu's clickOutside handler
    // The svar-ui ContextMenu listens for mousedown events to close the menu
    document.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
    // Call the parent's onSelectRow handler
    onSelectRow?.(ev);
  }, [onSelectRow]);

  // Build context menu options dynamically based on the selected row
  const menuOptions = useMemo(() => {
    // Find the selected row to check if it's a compressed chain
    const findRow = (nodes: TreeNode[] | undefined, id: string | number | null): TreeNode | null => {
      if (!nodes || !id) return null;
      for (const node of nodes) {
        if (node.id === id) return node;
        if (node.data?.length) {
          const found = findRow(node.data, id);
          if (found) return found;
        }
      }
      return null;
    };

    const selectedRow = findRow(data, selectedRowId);
    const isCompressedChain = Array.isArray(selectedRow?._chain) && (selectedRow?._chain?.length ?? 0) > 0;

    const options = [
      { id: "view-details", text: "View Details", icon: "wxi-eye" },
      { type: "separator" },
      { id: "collapse-siblings", text: "Collapse Siblings", icon: "wxi-fold" },
      { type: "separator" },
      { id: "focus", text: "Focus", icon: "wxi-target" },
      { 
        id: "clear-focus", 
        text: "Clear Focus", 
        icon: "wxi-close-circle",
        disabled: !focusId 
      },
    ];

    if (isCompressedChain) {
      options.push(
        { type: "separator" },
        { id: "expand-chain", text: "Expand Linear Chain", icon: "wxi-fullscreen" }
      );
    }

    return options;
  }, [data, selectedRowId, focusId]);

  // Handle context menu clicks
  const handleMenuClick = useCallback((ev: { action?: { id?: string } }) => {
    const option = ev.action;
    if (!option || !selectedRowId) return;

    const findRow = (nodes: TreeNode[] | undefined, id: string | number | null): TreeNode | null => {
      if (!nodes || !id) return null;
      for (const node of nodes) {
        if (node.id === id) return node;
        if (node.data?.length) {
          const found = findRow(node.data, id);
          if (found) return found;
        }
      }
      return null;
    };

    const row = findRow(data, selectedRowId);
    if (!row) return;

    switch (option.id) {
      case "view-details":
        onViewDetails?.(row);
        break;
      case "collapse-siblings":
        onCollapseSiblings?.(row.id);
        break;
      case "focus":
        onFocus?.(row.id);
        break;
      case "clear-focus":
        onClearFocus?.();
        break;
      case "expand-chain":
        onExpandChain?.(row.id);
        break;
    }
  }, [data, selectedRowId, onViewDetails, onCollapseSiblings, onFocus, onClearFocus, onExpandChain]);

  // Resolver to select the row and return its id for the context menu
  const resolver = useCallback((id: string | number) => {
    if (id) {
      // Select the row if not already selected
      gridApi?.exec("select-row", { id });
      setSelectedRowId(id);
    }
    return id;
  }, [gridApi]);

  // Handler for init callback - captures API and forwards to parent
  const handleInit = useCallback((api: GridApi) => {
    setGridApi(api);
    onInit?.(api);
  }, [onInit]);

  return (
    <section className={`app-grid-container${spanVizEnabled ? ` span-viz-${spanVizStyle}` : ""}${spanVizEnabled && spanVizColor ? ` span-viz-color-${spanVizColor}` : ""}`}>
      {isLoading && (
        <div className="grid-loading-overlay">
          <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
          <span className="text-sm text-muted-foreground mt-2">Loading trace data...</span>
        </div>
      )}
      {isEmpty && !isLoading && (
        <div className="grid-empty-state">
          <span className="text-sm text-muted-foreground">No data to display. Enter a search query above.</span>
        </div>
      )}
      <WillowDark>
        <ContextMenu
          api={gridApi as any}
          options={menuOptions}
          onClick={handleMenuClick as any}
          at="point"
          resolver={resolver}
          css="profiler-context-menu"
        >
          <Grid
            tree={true}
            data={data}
            columns={columns}
            sizes={{ rowHeight: 36 }}
            select={true}
            multiselect={true}
            rowStyle={rowStyle}
            columnStyle={columnStyle as any}
            init={handleInit as any}
            onOpenRow={onOpenRow}
            onCloseRow={onCloseRow}
            onSelectRow={handleSelectRow}
          />
        </ContextMenu>
      </WillowDark>
    </section>
  );
}
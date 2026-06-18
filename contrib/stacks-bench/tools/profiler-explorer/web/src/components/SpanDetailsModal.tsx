import React, { useEffect, useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "@/components/ui/dialog";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { Button } from "@/components/ui/button";
import { ChevronLeft, ChevronRight, Loader2 } from "lucide-react";
import { getRecordKv } from "@/lib/api.ts";
import { toMs } from "@/treeTransforms.ts";

/**
 * SpanDetailsModal - A modal dialog for viewing detailed span information
 * 
 * Props:
 * - open: boolean - Whether the modal is open
 * - onOpenChange: (open: boolean) => void - Callback when open state changes
 * - span: object | null - The span data to display
 * - onPrevious: () => void - Callback for navigating to the previous span
 * - onNext: () => void - Callback for navigating to the next span
 * - hasPrevious: boolean - Whether there is a previous span to navigate to
 * - hasNext: boolean - Whether there is a next span to navigate to
 * - numberFormat: object - Number formatting options
 */
interface RecordKvItem {
  key: string;
  value: string;
  value_type: string;
  count: number;
}

interface NumberFormatOption {
  id: string;
  label: string;
  group: string;
  decimal: string;
}

interface SpanData {
  id: string | number;
  span_name?: string;
  span_context?: string;
  tag?: string;
  parent_id?: string | number | null;
  depth?: number;
  call_count?: number;
  sample_count?: number;
  wall_time_us?: number;
  est_wall_us?: number;
  self_wall_time_us?: number;
  est_self_wall_us?: number;
  cpu_time_us?: number;
  est_cpu_us?: number;
  self_cpu_time_us?: number;
  est_self_cpu_us?: number;
  clarity_runtime_total?: number;
  clarity_runtime_avg?: number;
  clarity_read_count_total?: number;
  clarity_read_length_total?: number;
  clarity_write_count_total?: number;
  clarity_write_length_total?: number;
  clarity_input_n_total?: number;
  tx_hash_hex?: string;
  block_hash_hex?: string;
  contract_issuer?: string;
  contract?: string;
  contract_fn?: string;
  kv_total?: number;
  [key: string]: unknown;
}

interface SpanDetailsModalProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  span: SpanData | null;
  onPrevious: () => void;
  onNext: () => void;
  hasPrevious: boolean;
  hasNext: boolean;
  numberFormat: NumberFormatOption;
}

export default function SpanDetailsModal({
  open,
  onOpenChange,
  span,
  onPrevious,
  onNext,
  hasPrevious,
  hasNext,
  numberFormat,
}: SpanDetailsModalProps) {
  const [kvData, setKvData] = useState<RecordKvItem[]>([]);
  const [kvLoading, setKvLoading] = useState(false);
  const [kvError, setKvError] = useState<string | null>(null);

  // Fetch K/V pairs when span changes
  useEffect(() => {
    if (!span?.id || !open) {
      setKvData([]);
      return;
    }

    let cancelled = false;
    setKvLoading(true);
    setKvError(null);

    getRecordKv(span.id)
      .then((data) => {
        if (!cancelled) {
          setKvData(data);
          setKvLoading(false);
        }
      })
      .catch((err: Error) => {
        if (!cancelled) {
          setKvError(err.message);
          setKvLoading(false);
        }
      });

    return () => {
      cancelled = true;
    };
  }, [span?.id, open]);

  const formatNumber = (value: unknown, decimals = 3): string => {
    if (value == null || value === "-") return "-";
    const num = Number(value);
    if (!Number.isFinite(num)) return "-";

    if (numberFormat?.id === "compact") {
      return num.toLocaleString("en-US", {
        notation: "compact",
        maximumFractionDigits: decimals,
      });
    }
    return num.toLocaleString("en-US", {
      maximumFractionDigits: decimals,
      minimumFractionDigits: 0,
    });
  };

  const formatMs = (us: number | null | undefined): string => {
    const ms = toMs(us);
    if (ms == null) return "-";
    return formatNumber(ms, 3) + " ms";
  };

  // Build the detail rows for the Overview tab
  const overviewSections = span ? [
    {
      title: "Identity",
      rows: [
        { label: "Span Name", value: span.span_name || "-" },
        { label: "Span Context", value: span.span_context || "-" },
        { label: "Tag", value: span.tag || "-" },
        { label: "Record ID", value: span.id },
        { label: "Parent ID", value: span.parent_id ?? "(root)" },
        { label: "Depth", value: span.depth },
      ],
    },
    {
      title: "Call Statistics",
      rows: [
        { label: "Call Count", value: formatNumber(span.call_count, 0) },
        { label: "Sample Count", value: formatNumber(span.sample_count, 0) },
      ],
    },
    {
      title: "Wall Time",
      rows: [
        { label: "Inclusive (raw)", value: formatMs(span.wall_time_us) },
        { label: "Inclusive (estimated)", value: formatMs(span.est_wall_us) },
        { label: "Self (raw)", value: formatMs(span.self_wall_time_us) },
        { label: "Self (estimated)", value: formatMs(span.est_self_wall_us) },
      ],
    },
    {
      title: "CPU Time",
      rows: [
        { label: "Inclusive (raw)", value: formatMs(span.cpu_time_us) },
        { label: "Inclusive (estimated)", value: formatMs(span.est_cpu_us) },
        { label: "Self (raw)", value: formatMs(span.self_cpu_time_us) },
        { label: "Self (estimated)", value: formatMs(span.est_self_cpu_us) },
      ],
    },
    {
      title: "Clarity Costs",
      rows: [
        { label: "Runtime (total)", value: formatNumber(span.clarity_runtime_total, 0) },
        { label: "Runtime (avg)", value: formatNumber(span.clarity_runtime_avg, 2) },
        { label: "Read Count (total)", value: formatNumber(span.clarity_read_count_total, 0) },
        { label: "Read Length (total)", value: formatNumber(span.clarity_read_length_total, 0) },
        { label: "Write Count (total)", value: formatNumber(span.clarity_write_count_total, 0) },
        { label: "Write Length (total)", value: formatNumber(span.clarity_write_length_total, 0) },
        { label: "Input N (total)", value: formatNumber(span.clarity_input_n_total, 0) },
      ],
    },
    {
      title: "Transaction & Block",
      rows: [
        { label: "TX Hash", value: span.tx_hash_hex || "-" },
        { label: "Block Hash", value: span.block_hash_hex || "-" },
        { label: "Contract Issuer", value: span.contract_issuer || "-" },
        { label: "Contract", value: span.contract || "-" },
        { label: "Contract Function", value: span.contract_fn || "-" },
      ],
    },
  ] : [];

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="w-[90vw] max-w-[90vw] h-[85vh] max-h-[85vh] flex flex-col">
        <DialogHeader className="flex-shrink-0">
          <div className="flex items-center justify-between pr-8">
            <div className="flex-1 min-w-0">
              <DialogTitle className="text-lg font-semibold truncate">
                {span?.span_name || "Span Details"}
              </DialogTitle>
              <DialogDescription className="text-sm text-muted-foreground truncate">
                {span?.span_context || "View detailed span information and captured key/value pairs"}
              </DialogDescription>
            </div>
            <div className="flex items-center gap-1 ml-4 flex-shrink-0">
              <Button
                variant="outline"
                size="icon"
                onClick={onPrevious}
                disabled={!hasPrevious}
                title="Previous span"
              >
                <ChevronLeft className="h-4 w-4" />
              </Button>
              <Button
                variant="outline"
                size="icon"
                onClick={onNext}
                disabled={!hasNext}
                title="Next span"
              >
                <ChevronRight className="h-4 w-4" />
              </Button>
            </div>
          </div>
        </DialogHeader>

        <Tabs defaultValue="overview" className="flex-1 flex flex-col overflow-hidden">
          <TabsList className="flex-shrink-0">
            <TabsTrigger value="overview">Overview</TabsTrigger>
            <TabsTrigger value="kv">
              Recorded Data
              {(span?.kv_total ?? 0) > 0 && (
                <span className="ml-1.5 px-1.5 py-0.5 text-xs bg-muted rounded">
                  {span!.kv_total}
                </span>
              )}
            </TabsTrigger>
          </TabsList>

          <TabsContent value="overview" className="flex-1 overflow-auto mt-4">
            {span ? (
              <div className="space-y-6 pr-2">
                {overviewSections.map((section) => (
                  <div key={section.title} className="space-y-2">
                    <h3 className="text-sm font-semibold text-muted-foreground uppercase tracking-wider">
                      {section.title}
                    </h3>
                    <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
                      {section.rows.map((row) => (
                        <React.Fragment key={row.label}>
                          <span className="text-muted-foreground">{row.label}:</span>
                          <span className="font-mono">{row.value}</span>
                        </React.Fragment>
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            ) : (
              <div className="flex items-center justify-center h-full text-muted-foreground">
                No span selected
              </div>
            )}
          </TabsContent>

          <TabsContent value="kv" className="flex-1 overflow-auto mt-4">
            {kvLoading ? (
              <div className="flex items-center justify-center h-32">
                <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
                <span className="ml-2 text-sm text-muted-foreground">Loading recorded data...</span>
              </div>
            ) : kvError ? (
              <div className="flex items-center justify-center h-32 text-destructive">
                Error loading recorded data: {kvError}
              </div>
            ) : kvData.length === 0 ? (
              <div className="flex items-center justify-center h-32 text-muted-foreground">
                No data recorded for this span
              </div>
            ) : (
              <div className="pr-2">
                <table className="w-full text-sm">
                  <thead className="sticky top-0 bg-background border-b">
                    <tr>
                      <th className="text-left py-2 px-3 font-medium text-muted-foreground">Key</th>
                      <th className="text-left py-2 px-3 font-medium text-muted-foreground">Value</th>
                      <th className="text-left py-2 px-3 font-medium text-muted-foreground">Type</th>
                      <th className="text-right py-2 px-3 font-medium text-muted-foreground">Count</th>
                    </tr>
                  </thead>
                  <tbody>
                    {kvData.map((item, idx) => (
                      <tr key={`${item.key}-${item.value}-${idx}`} className="border-b border-muted/50 hover:bg-muted/20">
                        <td className="py-2 px-3 font-mono text-xs">{item.key}</td>
                        <td className="py-2 px-3 font-mono text-xs max-w-[400px] truncate" title={item.value}>
                          {item.value}
                        </td>
                        <td className="py-2 px-3 text-muted-foreground">{item.value_type}</td>
                        <td className="py-2 px-3 text-right font-mono">{formatNumber(item.count, 0)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}

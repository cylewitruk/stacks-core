export type ApiParams = Record<string, string | number | boolean | object | Array<string | number> | undefined | null>;

export interface FetchOptions {
  signal?: AbortSignal;
}

export function buildApiUrl(path: string, params: ApiParams = {}): URL {
  const url = new URL(path, window.location.origin);
  Object.entries(params).forEach(([key, value]) => {
    if (value == null || value === "") return;
    if (Array.isArray(value)) {
      if (value.length === 0) return;
      url.searchParams.set(key, value.map((item) => String(item)).join(","));
      return;
    }
    if (typeof value === "object") {
      url.searchParams.set(key, JSON.stringify(value));
      return;
    }
    url.searchParams.set(key, String(value));
  });
  return url;
}

export async function fetchJson<T = unknown>(url: URL, { signal }: FetchOptions = {}): Promise<T> {
  const response = await fetch(url.toString(), { signal });
  if (!response.ok) {
    const text = await response.text().catch(() => "");
    throw new Error(text || `HTTP ${response.status}`);
  }
  return response.json() as Promise<T>;
}

export function getRuns<T = unknown>() {
  return fetchJson<T>(buildApiUrl("/api/runs"));
}

export function getBlocks<T = unknown>(runId: string | number) {
  return fetchJson<T>(buildApiUrl("/api/blocks", { run_id: runId }));
}

export function lookupTx<T = unknown>(runId: string | number, txHash: string) {
  return fetchJson<T>(buildApiUrl("/api/tx-lookup", { run_id: runId, tx_hash: txHash }));
}

export function getTrace<T = unknown>(params: ApiParams, options: FetchOptions = {}) {
  return fetchJson<T>(buildApiUrl("/api/trace", params), options);
}

export function getTransactions<T = unknown>(params: ApiParams, options: FetchOptions = {}) {
  return fetchJson<T>(buildApiUrl("/api/transactions", params), options);
}

export function getTransactionsMaxes<T = unknown>(params: ApiParams, options: FetchOptions = {}) {
  return fetchJson<T>(buildApiUrl("/api/transactions/maxes", params), options);
}

export function getTransactionsAutocomplete<T = unknown>(params: ApiParams, options: FetchOptions = {}) {
  return fetchJson<T>(buildApiUrl("/api/transactions/autocomplete", params), options);
}

export function getTxTypes<T = unknown>(options: FetchOptions = {}) {
  return fetchJson<T>(buildApiUrl("/api/tx-types"), options);
}

export type RecordKvItem = {
  key: string;
  value: string;
  value_type: string;
  count: number;
};

export function getRecordKv(recordId: string | number, options: FetchOptions = {}) {
  return fetchJson<RecordKvItem[]>(buildApiUrl(`/api/record/${recordId}/kv`), options);
}

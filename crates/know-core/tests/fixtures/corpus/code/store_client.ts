/**
 * Zvec store client for the smoke corpus: demonstrates TypeScript AST
 * chunking (interfaces and exported functions are chunk boundaries).
 */

export interface SearchHit {
  kind: "archive";
  score: number;
  path: string;
  start_line: number;
  end_line: number;
  title: string;
}

export interface SearchParams {
  query: string;
  mode?: "auto" | "keyword" | "vector" | "hybrid";
  topK?: number;
  pathFilter?: string;
}

/** searchArchive runs a hybrid query against the zvec collection. */
export async function searchArchive(params: SearchParams): Promise<SearchHit[]> {
  const response = await fetch("http://127.0.0.1:8765/admin", {
    method: "POST",
    body: JSON.stringify({ method: "search", params }),
  });
  if (!response.ok) {
    throw new Error(`search failed: ${response.status}`);
  }
  const { result } = await response.json();
  return result.hits as SearchHit[];
}

/** readWithLines fetches a numbered excerpt of an indexed file. */
export async function readWithLines(path: string, startLine = 1, endLine = 2000): Promise<string> {
  const response = await fetch("http://127.0.0.1:8765/admin", {
    method: "POST",
    body: JSON.stringify({ method: "read", params: { path, start_line: startLine, end_line: endLine } }),
  });
  return response.text();
}

/** debounce guards the search box from firing on every keystroke. */
export function debounce<A extends unknown[]>(fn: (...args: A) => void, ms: number) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return (...args: A) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
}

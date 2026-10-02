import { displayPathText } from "./paths.js";

export function memoryMatchesQuery(memory, query) {
  const needle = displayPathText(query).toLowerCase();
  return [
    memory.id,
    memory.key,
    memory.title,
    memory.summary,
    memory.status,
    memory.revision,
    ...(memory.tags || []),
  ].some((value) => displayPathText(value).toLowerCase().includes(needle));
}

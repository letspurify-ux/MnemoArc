// Progress of a running task's tool calls. The runtime sends a one-line
// summary of each call (tools::call_summary): what it reads, searches or
// changes. Older runtimes send only the tool names.

/** Steps named in the progress line; a larger parallel batch is counted. */
const SHOWN_STEPS = 3;

/** The progress line while a tool batch runs. */
export function toolProgress(activity, labels) {
  const steps = (activity?.steps || []).filter(
    (step) => typeof step === "string" && step,
  );
  const names = (activity?.tools || [])
    .filter((name) => typeof name === "string" && name)
    .map((name) => labels[name] || name);
  const shown = steps.length ? steps : names;
  if (!shown.length) return "도구 실행 중";
  const more =
    shown.length > SHOWN_STEPS ? ` 외 ${shown.length - SHOWN_STEPS}건` : "";
  return `${shown.slice(0, SHOWN_STEPS).join(" · ")}${more} 실행 중`;
}

/** Finished calls shown below the progress line, oldest first. */
export function recentSteps(activity) {
  return (activity?.recent || [])
    .filter((step) => typeof step?.text === "string" && step.text)
    .map((step) => ({ text: step.text, ok: step.status === "ok" }));
}

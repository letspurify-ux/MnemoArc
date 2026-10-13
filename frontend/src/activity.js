// Progress of a running task's tool calls. The runtime sends a one-line
// summary of each call (tools::call_summary): what it reads, searches or
// changes. Older runtimes send only the tool names.

/** The progress line while a tool batch runs. */
export function toolProgress(activity, labels) {
  const steps = (activity?.steps || []).filter(
    (step) => typeof step === "string" && step,
  );
  const shown = steps.length
    ? steps
    : (activity?.tools || []).map((name) => labels[name] || name);
  return `${shown.join(" · ")} 실행 중`;
}

/** Finished calls shown below the progress line, oldest first. */
export function recentSteps(activity) {
  return (activity?.recent || [])
    .filter((step) => typeof step?.text === "string" && step.text)
    .map((step) => ({ text: step.text, ok: step.status === "ok" }));
}

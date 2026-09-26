// Keep transport chunks separate in history, but render one Markdown answer.
export function continuationMessages(bundles, stream, pending) {
  const messages = [];
  for (const bundle of bundles || []) {
    for (const [index, message] of bundle.messages.entries()) {
      const current = { ...message, key: `${bundle.id}-${index}` };
      const previous = messages.at(-1);
      if (
        current.continues_previous &&
        !current.follow_up &&
        !previous?.follow_up &&
        previous?.partial &&
        current.role === "assistant" &&
        previous.role === "assistant" &&
        !current.tool_calls?.length &&
        !previous.tool_calls?.length
      ) {
        previous.content = (previous.content || "") + (current.content || "");
        previous.partial = !!current.partial;
      } else {
        messages.push(current);
      }
    }
  }
  let streamText = stream || "";
  const previous = messages.at(-1);
  if (
    streamText &&
    pending &&
    !previous?.follow_up &&
    previous?.partial &&
    previous.role === "assistant"
  ) {
    streamText = (previous.content || "") + streamText;
    messages.pop();
  }
  return { messages, streamText };
}

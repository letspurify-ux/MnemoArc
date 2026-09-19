import { normalizeSerializedMarkdown } from '../../shared/serialized-markdown.mjs';
import { fromMarkdown } from 'mdast-util-from-markdown';
export { decodeSerializedLines, decodeVisualizationBreaks, decodeMathEscapes } from '../../shared/serialized-markdown.mjs';

export default function remarkSerializedMarkdown() {
  const processor = this;
  return (tree, file) => {
    const source = String(file);
    const normalized = normalizeSerializedMarkdown(source, tree, value => processor.parse(value), value => fromMarkdown(value, {
      extensions: [...processor.data('micromarkExtensions'), { disable: { null: ['table'] } }],
      mdastExtensions: processor.data('fromMarkdownExtensions'),
    }));
    if (normalized === source) return tree;
    file.value = normalized;
    return processor.parse(normalized);
  };
}

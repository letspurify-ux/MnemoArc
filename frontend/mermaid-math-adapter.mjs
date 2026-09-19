// Mermaid 11.17.2의 공통 수식 확장 지점. 배포/개발 사전 번들에 같은 변경을
// 적용하며, 설치 버전이나 함수 형태가 바뀌면 조용히 원래 경로로 돌아가지 않는다.
import { createRequire } from 'node:module';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { createHash } from 'node:crypto';
import { MATH_ADAPTER_KEY } from './shared/mermaid-math-spans.mjs';

const require = createRequire(import.meta.url);
const definitions = [
  [/var hasKatex = [^\n]+, "hasKatex"\);/g,
    `var hasKatex = text => globalThis[Symbol.for(${JSON.stringify(MATH_ADAPTER_KEY)})].hasMath(text);`],
  [/var renderKatexUnsanitized = [\s\S]+?\}, "renderKatexUnsanitized"\);/g,
    `var renderKatexUnsanitized = async text => globalThis[Symbol.for(${JSON.stringify(MATH_ADAPTER_KEY)})].renderLabel(text);`],
];

export function adaptMermaidMath(code) {
  for (const [pattern, replacement] of definitions) {
    if ([...code.matchAll(pattern)].length !== 1) throw new Error('Mermaid 수식 연결 지점이 변경됐습니다. 어댑터와 회귀 검사를 검토해야 합니다.');
    code = code.replace(pattern, () => replacement);
  }
  return code;
}

export function adaptMermaidLabels(code) {
  const call = 'renderKatexSanitized(node.label.replace(common_default.lineBreakRegex, "\\n"), config)';
  if (code.split(call).length !== 2) throw new Error('Mermaid 라벨 개행 연결 지점이 변경됐습니다. 어댑터를 검토해야 합니다.');
  // br 변환도 수식 엔진이 소유한다. 이 호출 전에 전역 변환하면
  // \text{a<br>b}의 글자를 엔진에 전달하기도 전에 잃는다.
  return code.replace(call, 'renderKatexSanitized(node.label, config)');
}

export function installedMermaidMath() {
  const packagePath = require.resolve('mermaid/package.json');
  const { version } = JSON.parse(readFileSync(packagePath, 'utf8'));
  if (version !== '11.17.2') throw new Error(`Mermaid ${version}의 수식 어댑터를 검토해야 합니다 (검증 버전 11.17.2).`);
  const directory = join(dirname(packagePath), 'dist/chunks/mermaid.core');
  const files = readdirSync(directory).filter(name => name.endsWith('.mjs')).map(name => join(directory, name));
  const candidates = files.filter(path => readFileSync(path, 'utf8').includes('var renderKatexUnsanitized ='));
  if (candidates.length !== 1) throw new Error('Mermaid 공통 수식 모듈을 하나로 식별하지 못했습니다.');
  const path = candidates[0], code = readFileSync(path, 'utf8');
  const labels = files.filter(path => readFileSync(path, 'utf8').includes('renderKatexSanitized(node.label.replace'));
  if (labels.length !== 1) throw new Error('Mermaid 라벨 개행 모듈을 하나로 식별하지 못했습니다.');
  return { path, code: adaptMermaidMath(code), labelPath: labels[0], labelCode: adaptMermaidLabels(readFileSync(labels[0], 'utf8')) };
}

export function mermaidMathAdapter() {
  const { path, code, labelPath, labelCode } = installedMermaidMath();
  // Vite의 모듈 ID는 Windows에서도 /를 쓰며 esbuild의 파일 경로는 \를 쓴다.
  const normalizePath = path => path.replaceAll('\\', '/');
  const modules = new Map([[normalizePath(path), code], [normalizePath(labelPath), labelCode]]);
  // Vite는 esbuild 플러그인 함수 본문을 의존성 캐시 키에 넣지 않는다.
  // 실제 변환 결과를 키에 포함해 어댑터 수정 뒤에도 낡은 수식 경로가 남지 않게 한다.
  const revision = createHash('sha256').update(code).update(labelCode).digest('hex');
  return {
    name: 'shared-mermaid-math-v1',
    enforce: 'pre',
    config: () => ({ optimizeDeps: { esbuildOptions: {
      define: { __LLM_MERMAID_MATH_ADAPTER__: JSON.stringify(revision) },
      plugins: [{
        name: 'shared-mermaid-math-v1',
        setup(build) {
          build.onLoad({ filter: /mermaid[/\\]dist[/\\].*\.mjs$/ }, args => {
            const code = modules.get(normalizePath(args.path));
            return code === undefined ? undefined : { contents: code, loader: 'js' };
          });
        },
      }],
    } } }),
    transform(_code, id) {
      const code = modules.get(normalizePath(id.split('?')[0]));
      return code === undefined ? null : { code, map: null };
    },
  };
}

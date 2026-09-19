# 채팅 코드 재사용

원본: `/Users/iceblue/workspace/llm_agent/frontend` 및 그 저장소의 `shared` 디렉터리. 원본은 수정하지 않았습니다.

- `src/chat/Message.jsx`: 원본 `App.jsx`에서 메시지, 스트리밍 Markdown 미리보기, 표·차트·수식·Mermaid, 링크 정책과 렌더 오류 경계를 추출했습니다.
- `src/chat/*`, `shared/*`, `mermaid-math-adapter.mjs`: 렌더링 의존 코드를 복사했습니다. 상대 경로만 조정했습니다.
- `src/chat/chat.css`: 원본 HTML의 채팅 스타일입니다. MnemoArc 화면 구성은 별도 `src/styles.css`에서 적용합니다.
- `src/Chat.jsx`: 기존 입력창의 높이 조절, 한글 조합 처리, 중복 전송 방지 방식을 MnemoArc 세션 API에 맞춰 적용했습니다.

원본 서비스의 운영 DB·관리자 기능과 대화 이력 전송 API는 MnemoArc와 계약이 다르므로 가져오지 않았습니다. 대화 원문은 Rust 세션이 소유합니다.

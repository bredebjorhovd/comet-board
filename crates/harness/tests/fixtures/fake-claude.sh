#!/bin/sh
# Fake Claude Code CLI for comet-harness tests.
#
# Reads the first stream-json user line from stdin, picks a scenario from the
# prompt text, and plays a scripted stream-json transcript on stdout —
# including control-channel round-trips read back from stdin. Driven by
# crates/harness/tests/claude.rs.

read -r first || exit 1

emit() { printf '%s\n' "$1"; }

case "$first" in

*scenario:happy*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash","Read"],"cwd":"/tmp","session_id":"sess-1"}'
  # Re-emitted init mid-run (background-task wakeup): must be deduped.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash","Read"],"cwd":"/tmp","session_id":"sess-1"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"pondering"}}}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}}'
  # The parent delegates; every frame below carries the Task's id as parent.
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg-main-1","model":"claude-fable-5","usage":{"input_tokens":4,"output_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000},"content":[{"type":"tool_use","id":"sub-1","name":"Task","input":{"description":"scan the tree","subagent_type":"Explore","prompt":"a long brief"}}]}}'
  # Subagent frames (parent_tool_use_id set): never parent transcript content —
  # the token stream beats as liveness, its tool calls count as steps (gh#280).
  emit '{"type":"stream_event","parent_tool_use_id":"sub-1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"SUBAGENT"}}}'
  emit '{"type":"assistant","parent_tool_use_id":"sub-1","message":{"id":"msg-sub-1","model":"claude-spark-5","usage":{"input_tokens":2,"output_tokens":4,"cache_creation_input_tokens":50,"cache_read_input_tokens":1500},"content":[{"type":"tool_use","id":"sub-tool","name":"Bash","input":{"command":"echo sub"}}]}}'
  emit '{"type":"user","parent_tool_use_id":"sub-1","message":{"content":[{"type":"tool_result","tool_use_id":"sub-tool","is_error":false}]}}'
  emit '{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"sub-1","is_error":false}]}}'
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg-main-2","model":"claude-fable-5","usage":{"input_tokens":4,"output_tokens":14,"cache_creation_input_tokens":150,"cache_read_input_tokens":1500},"content":[{"type":"text","text":"Hello"},{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"ls -la"}},{"type":"tool_use","id":"tool-2","name":"mcp__linear__search","input":{"q":"bug"}}]}}'
  emit '{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":false},{"type":"tool_result","tool_use_id":"tool-2","is_error":true}]}}'
  # Informational rate-limit status: stays quiet.
  emit '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}'
  emit '{"type":"result","subtype":"success","result":"done!","errors":[],"usage":{"input_tokens":10,"output_tokens":20,"cache_creation_input_tokens":300,"cache_read_input_tokens":4000},"modelUsage":{"claude-fable-5":{"inputTokens":8,"outputTokens":16,"cacheCreationInputTokens":250,"cacheReadInputTokens":2500},"claude-spark-5":{"inputTokens":2,"outputTokens":4,"cacheCreationInputTokens":50,"cacheReadInputTokens":1500}},"session_id":"sess-1","total_cost_usd":0.01}'
  ;;

*scenario:askuser*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-ask"}'
  # A plain tool permission request: must be auto-allowed.
  emit '{"type":"control_request","request_id":"cr-0","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}'
  read -r resp0 || exit 1
  case "$resp0" in
  *'"request_id":"cr-0"'*'"behavior":"allow"'*) ;;
  *)
    emit '{"type":"result","subtype":"error_during_execution","errors":["bash tool was not allowed"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
    exit 0
    ;;
  esac
  # AskUserQuestion: must be intercepted and answered via updatedInput.answers.
  emit '{"type":"control_request","request_id":"cr-1","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"header":"Choice","question":"Pick one","options":["A","B"],"multiSelect":false}]}}}'
  read -r resp1 || exit 1
  case "$resp1" in
  *'"behavior":"allow"'*)
    case "$resp1" in
    *'"Pick one":"B"'*)
      emit '{"type":"result","subtype":"success","result":"answered","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
      ;;
    *)
      emit '{"type":"result","subtype":"error_during_execution","errors":["answers missing from updatedInput"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
      ;;
    esac
    ;;
  *)
    emit '{"type":"result","subtype":"error_during_execution","errors":["AskUserQuestion was denied"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ask"}'
    ;;
  esac
  ;;

*scenario:context-unsupported*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-ctx-no"}'
  read -r ctx0 || exit 1
  id=$(printf '%s\n' "$ctx0" | sed 's/.*"request_id":"\([^"]*\)".*/\1/')
  # What an older CLI (or a remote thin client) answers: no callback for it.
  emit "{\"type\":\"control_response\",\"response\":{\"subtype\":\"error\",\"request_id\":\"$id\",\"error\":\"get_context_usage is not supported in this context\"}}"
  emit '{"type":"result","subtype":"success","result":"fine without it","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ctx-no"}'
  ;;

*scenario:context*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-ctx"}'
  # The harness polls the control channel for context fullness (gh#271).
  read -r ctx0 || exit 1
  case "$ctx0" in
  *'"subtype":"get_context_usage"'*)
    id=$(printf '%s\n' "$ctx0" | sed 's/.*"request_id":"\([^"]*\)".*/\1/')
    emit "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$id\",\"response\":{\"totalTokens\":120000,\"maxTokens\":200000,\"rawMaxTokens\":200000,\"percentage\":60,\"autoCompactThreshold\":167000,\"isAutoCompactEnabled\":true}}}"
    ;;
  *)
    emit '{"type":"result","subtype":"error_during_execution","errors":["expected a get_context_usage control request"],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ctx"}'
    exit 0
    ;;
  esac
  # A second poll, further along — the level moved, so it is reported again.
  read -r ctx1 || exit 1
  id=$(printf '%s\n' "$ctx1" | sed 's/.*"request_id":"\([^"]*\)".*/\1/')
  emit "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$id\",\"response\":{\"totalTokens\":170000,\"maxTokens\":200000,\"rawMaxTokens\":200000,\"percentage\":85,\"autoCompactThreshold\":167000,\"isAutoCompactEnabled\":true}}}"
  # …and a third, unchanged: a level that did not move is not news.
  read -r ctx2 || exit 1
  id=$(printf '%s\n' "$ctx2" | sed 's/.*"request_id":"\([^"]*\)".*/\1/')
  emit "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$id\",\"response\":{\"totalTokens\":170000,\"maxTokens\":200000,\"rawMaxTokens\":200000,\"percentage\":85,\"autoCompactThreshold\":167000,\"isAutoCompactEnabled\":true}}}"
  emit '{"type":"result","subtype":"success","result":"full","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-ctx"}'
  # A reply that lands AFTER the turn ended (a poll in flight when the result
  # frame went out). It must never reach the stream: a run journal whose last
  # event is not a `Done` reads as a run that died mid-stream.
  emit '{"type":"control_response","response":{"subtype":"success","request_id":"ctx_99","response":{"totalTokens":199000,"maxTokens":200000,"rawMaxTokens":200000,"percentage":100,"autoCompactThreshold":167000,"isAutoCompactEnabled":true}}}'
  ;;

*scenario:steer*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-steer"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"first"}}}'
  # The queued steering user line, applied at "the step boundary" (here: now).
  read -r steer || exit 1
  content=$(printf '%s\n' "$steer" | sed 's/.*"content":"\([^"]*\)".*/\1/')
  emit "{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"steered:$content\"}}}"
  emit '{"type":"result","subtype":"success","result":"steered","errors":[],"usage":{"input_tokens":1,"output_tokens":1},"session_id":"sess-steer"}'
  ;;

*scenario:interrupt*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-int"}'
  # Wedge without reading stdin — forces the SIGTERM escalation path.
  exec sleep 30
  ;;

*scenario:error*)
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-err"}'
  # Terse assistant-level error code with no content.
  emit '{"type":"assistant","parent_tool_use_id":null,"message":{"content":[]},"error":"rate_limit"}'
  # Hard-rejected claude.ai usage window.
  emit '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}'
  # Result error with an EMPTY errors array: needs fallback wording.
  emit '{"type":"result","subtype":"error_max_turns","errors":[],"usage":{"input_tokens":1,"output_tokens":2},"session_id":"sess-err"}'
  ;;

*scenario:oomkill*)
  # A memory hog the kernel takes mid-turn (gh#533). Stands in for the real
  # thing — a `cargo`/`next build` child whose allocation the OOM killer
  # answers with SIGKILL — because the arrival is what the harness sees and it
  # is identical: some output, then the process is gone on signal 9, with no
  # result frame and no chance to say anything.
  emit '{"type":"system","subtype":"init","model":"claude-fable-5","tools":["Bash"],"cwd":"/tmp","session_id":"sess-oom"}'
  emit '{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Building"}}}'
  # Nothing on stderr, deliberately: SIGKILL is not deliverable and not
  # catchable, so a process the OOM killer chooses gets no chance to explain
  # itself. That silence is the whole difficulty — it is why the death arrives
  # as a bare signal number and why the board has to attribute it from outside.
  kill -9 $$
  ;;

*)
  emit '{"type":"result","subtype":"error_during_execution","errors":["unknown scenario"],"usage":{"input_tokens":0,"output_tokens":0},"session_id":"sess-x"}'
  ;;
esac

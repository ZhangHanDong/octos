spec: task
name: "OLP 观测面阶段 2:events.jsonl 新增 fallback_switch 与 malformed_exhausted 两个发射点"
tags: [olp, observability, obs-events, octos-cli, octos-agent]
estimate: 1d
---

## Intent

进化环(octoscode LEP-003)的采集哨消费 `events.jsonl`,但两类内环摩擦今天没有事件:模型
车道 failover(RouterFailoverEvent 只推给客户端做协议通知)与 malformed tool-call 自纠预算
耗尽(agent 侧 3 次后落到通用错误,CLI 只发 `turn_error`,detail 是原始错误文本)。本任务给
`events.jsonl` 增加两个 kind 的发射点,沿用 `obs_events.rs` 的 best-effort 语义,不改任何既有
事件的字段与语义,不改 wire 协议。对应 octoscode 侧 REQ-OLP-OBS 白名单修订。

## Decisions

<!-- lint-ack: decision-coverage — 白名单文档注释更新由"事件流文档"场景整体行使 -->

- `fallback_switch`:在 `crates/octos-cli/src/session_actor.rs` 的 `forward_router_failovers`
  中,事件通过本会话过滤(`originating_session_id == session_id`)之后、debounce 之前,追加
  `ObsEvent::new("fallback_switch", &detail).session(Some(&session_id))
  .model_lane(Some(&event.to_provider))`,`detail` 恰为
  `router failover: <from_provider> -> <to_provider> (<reason>, <elapsed_ms>ms)`;被 debounce
  抑制的客户端通知不影响事件写入(事件每次真实切道都写)。`FailoverForwarderParams` 新增
  `profile_data_dir: PathBuf`,由 spawn 处(`session_actor.rs` 约 L5789)从 actor 已持有的
  profile data_dir 传入;其它会话的事件仍被丢弃且不写事件。
- `malformed_exhausted`:`crates/octos-agent/src/agent/loop_runner.rs` 在
  `malformed_feedback_used > MALFORMED_TOOLCALL_FEEDBACK_LIMIT` 的分支,把返回给上层的错误用
  `anyhow::Context`/`wrap_err` 加上稳定前缀 `MALFORMED_TOOLCALL_EXHAUSTED_MARKER`(新增
  `pub const` 于 `octos_agent` crate 根,值 `malformed tool-call feedback budget exhausted`),
  文案形如 `<marker> (3/3): <原错误>`;`crates/octos-cli/src/api/ui_protocol_transport.rs` 现有
  `turn_error` 发射点(约 L32599)在 `detail` 含该 marker 时**额外**追加一条
  `ObsEvent::new("malformed_exhausted", "attempts=3/3").session(...)`(goal_id/slug 与
  `turn_error` 同源),`turn_error` 本身照常发射。
- 未耗尽(≤ 3 次自纠成功续跑)不发 `malformed_exhausted`;既有的自纠反馈行为不变。
- `obs_events.rs` 顶部文档注释的 kind 清单追加两个新 kind;不改 `ObsEvent` 结构与序列化。
- 测试:`fallback_switch` 用 `forward_router_failovers` 的既有测试夹具(广播 `FailoverEvent` +
  临时 data_dir)断言 `events.jsonl` 新增一行且字段正确、他会话事件零写入;
  `malformed_exhausted` 在 octos-agent 用现有 loop_runner 测试夹具断言耗尽错误含 marker,
  在 octos-cli 用 `turn_error` 发射点的既有测试夹具(注入含 marker 的错误)断言两行事件。
  不引入新依赖。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/session_actor.rs
- crates/octos-cli/src/api/ui_protocol_transport.rs
- crates/octos-cli/src/obs_events.rs
- crates/octos-agent/src/agent/loop_runner.rs
- crates/octos-agent/src/lib.rs

### Forbidden
- 不改既有六种 kind 的字段、detail 文案与发射时机。
- 不改 `RouterFailoverEvent` 的 wire 形状与客户端通知逻辑(debounce 规则不动)。
- 不改 `MALFORMED_TOOLCALL_FEEDBACK_LIMIT` 的值与自纠反馈文案。
- 不引入新依赖;不改 events.jsonl 的只追加语义。

## Out of Scope

- 采集哨对新 kind 的识别(octoscode 侧,阶段 2 契约 B)。
- events.jsonl 轮转。
- 其它候选 kind(如 budget checkpoint 通知)。

## Acceptance Criteria

Scenario: 本会话 failover 写入 fallback_switch 事件(critical)
  Tags: critical
  Test: obs_fallback_switch_event_written_for_own_session
  Given 转发器以临时 data_dir 启动且广播一条 originating_session_id 为本会话、from a、to b、reason quota、elapsed 120 的 FailoverEvent
  When 转发器处理该事件
  Then events.jsonl 新增一行 kind 为 fallback_switch
  And 该行 session 等于本会话 id、model_lane 等于 b、detail 等于 "router failover: a -> b (quota, 120ms)"

Scenario: 他会话 failover 不写事件
  Test: obs_fallback_switch_ignores_other_session
  Given 转发器收到 originating_session_id 为其它会话的 FailoverEvent
  When 转发器处理该事件
  Then events.jsonl 不新增任何行

Scenario: debounce 抑制通知但不抑制事件
  Test: obs_fallback_switch_written_even_when_notice_debounced
  Given 转发器在 debounce 窗口内连续收到两条本会话 FailoverEvent
  When 转发器处理两条事件
  Then events.jsonl 新增两行 fallback_switch
  And 客户端通知只发送一条

Scenario: 自纠预算耗尽的错误带稳定 marker
  Test:
    Package: octos-agent
    Filter: malformed_exhaustion_error_carries_marker
  Given 模型连续四次返回 MalformedArgs
  When loop_runner 处理该 turn
  Then 返回的错误文本以 MALFORMED_TOOLCALL_EXHAUSTED_MARKER 开头并含 "(3/3)"

Scenario: 耗尽时 CLI 额外发射 malformed_exhausted(critical)
  Tags: critical
  Test: obs_malformed_exhausted_event_alongside_turn_error
  Given turn 以含 marker 的错误结束
  When turn_error 发射点处理该错误
  Then events.jsonl 新增两行,kind 分别为 turn_error 与 malformed_exhausted
  And malformed_exhausted 行 detail 等于 "attempts=3/3" 且 session 与 turn_error 行相同

Scenario: 未耗尽不发射 malformed_exhausted
  Test: obs_no_malformed_exhausted_below_limit
  Given turn 以不含 marker 的普通错误结束
  When turn_error 发射点处理该错误
  Then events.jsonl 只新增一行 kind 为 turn_error

Scenario: 事件流文档列出新 kind
  Test: obs_events_doc_lists_new_kinds
  Given 仓库检出
  When 读取 crates/octos-cli/src/obs_events.rs 的顶部注释
  Then 注释含 fallback_switch 与 malformed_exhausted

spec: task
name: "OLP 观测面阶段 2:events.jsonl 新增 fallback_switch 与 malformed_exhausted 两个发射点"
tags: [olp, observability, obs-events, octos-cli, octos-agent]
estimate: 1.5d
---

## Intent

进化环(octoscode LEP-003)的采集哨消费 `events.jsonl`,但两类内环摩擦今天没有事件:模型
车道 failover(`RouterFailoverEvent` 只推给客户端做协议通知,gateway 与 serve/UI 两条转发路径都
不写事件)与 malformed tool-call 自纠预算耗尽(agent 侧超过 3 次后落入通用错误分派,CLI 的
`events.jsonl` 今日只在连接断开路径写 `turn_error`,agent 失败终态不写事件)。本任务给
`events.jsonl` 增加两个 kind 的发射点,沿用 `obs_events.rs` 的 best-effort 语义,不改既有六种
kind 的字段、文案与发射时机,不改 wire 协议。契约 v2 已并入 codex/grok 对抗复审。

## Decisions

<!-- lint-ack: decision-coverage — 白名单文档注释更新由"事件流文档"场景整体行使 -->

- `fallback_switch` 在**两条**转发路径都写(缺一条则 octoscode 内环主路径采集为空):
  ① gateway:`crates/octos-cli/src/session_actor.rs` 的 `forward_router_failovers`,在
  `originating_session_id == Some(session_id)` 过滤成功之后、`last_push` debounce 判定之前;
  `FailoverForwarderParams` 新增 `profile_data_dir: PathBuf`,spawn 处以 `self.data_dir.clone()`
  传入。② serve/UI:`crates/octos-cli/src/api/ui_protocol_transport.rs` 的
  `spawn_router_failover_forwarder`,在其本会话过滤成功之后、`send_notification_durable` 之前;
  data_dir 取 `ledger.config_data_dir()`,取不到则跳过写入。两条路径对 `originating_session_id`
  为 `None` 或他会话的事件都 MUST NOT 写。
- `fallback_switch` 事件形状:`ObsEvent::new("fallback_switch", &detail).session(Some(&session_id))
  .model_lane(Some(&event.to_provider))`,`detail` 恰为
  `router failover: <from_provider> -> <to_provider> (<reason>, <elapsed_ms>ms)`。语义是
  best-effort:写的是"本接收器成功收到且本会话匹配"的事件,broadcast `Lagged(n)` 丢失的事件不
  补;写失败(data_dir 不可写)不得阻断客户端通知。
- `malformed_exhausted`:`crates/octos-agent/src/lib.rs` 新增
  `pub const MALFORMED_TOOLCALL_EXHAUSTED_MARKER: &str = "malformed tool-call feedback budget exhausted";`;
  `crates/octos-agent/src/agent/loop_runner.rs` 在 `malformed_feedback_used > MALFORMED_TOOLCALL_FEEDBACK_LIMIT`
  分支构造 `eyre!("{MARKER} feedback_limit={LIMIT} observed_malformed={used}: {e:#}")` 并
  **直接 `return Err(...)`**,不再进入 `handle_loop_error_with_dispatch`/failfast 分派;未耗尽的
  反馈文案与行为不变。
- CLI 侧:`ui_protocol_transport.rs` 的 agent 失败终态路径(`TerminalReason::Errored` →
  `send_turn_error` 所在分支)在错误文本 `starts_with(MARKER)` 时追加一条
  `ObsEvent::new("malformed_exhausted", "feedback_limit=3 observed_malformed=4").session(Some(&session))`
  (data_dir 取 `ledger.config_data_dir()`);**只用 `starts_with`**,不用任意位置 `contains`;
  该路径**不**新增 `turn_error` 事件行(既有连接断开路径的 `turn_error` 发射点不动),以免改变
  既有 kind 的发射时机。
- `obs_events.rs` 顶部文档注释的 kind 清单追加两个新 kind;不改 `ObsEvent` 结构与序列化。
- 测试(全部为**新建**,提交树上没有可直接复用的 forwarder+data_dir 或 marker→事件夹具):
  gateway 路径在 `crates/octos-cli/src/session_actor_tests.rs` 用 broadcast `FailoverEvent` +
  临时 data_dir;UI 路径在 `crates/octos-cli/src/api/ui_protocol_tests.rs` 驱动
  `spawn_router_failover_forwarder` 或其被测内核;agent 侧复用 `MalformedThenOkProvider` 但按契约
  测试名新建;CLI 侧新建"agent 连续四次 MalformedArgs → 终态 → events.jsonl"端到端测试。
  不引入新依赖。

## Boundaries

### Allowed Changes
- crates/octos-cli/src/session_actor.rs
- crates/octos-cli/src/session_actor_tests.rs
- crates/octos-cli/src/api/ui_protocol_transport.rs
- crates/octos-cli/src/api/ui_protocol_tests.rs
- crates/octos-cli/src/obs_events.rs
- crates/octos-agent/src/agent/loop_runner.rs
- crates/octos-agent/src/agent/loop_runner_tests.rs
- crates/octos-agent/src/lib.rs

### Forbidden
- 不改既有六种 kind 的字段、detail 文案与发射时机(含连接断开路径的 `turn_error`)。
- 不改 `RouterFailoverEvent` 的 wire 形状与客户端通知逻辑(debounce 规则不动)。
- 不改 `MALFORMED_TOOLCALL_FEEDBACK_LIMIT` 的值与未耗尽时的自纠反馈文案。
- 不引入新依赖;不改 events.jsonl 的只追加语义。

## Out of Scope

- 采集哨对新 kind 的识别(octoscode 侧)。
- events.jsonl 轮转;broadcast Lagged 的补偿。
- 其它候选 kind。

## Acceptance Criteria

Scenario: gateway 路径本会话 failover 写入事件(critical)
  Tags: critical
  Test: obs_fallback_switch_gateway_writes_own_session
  Given forward_router_failovers 以临时 data_dir 启动且广播一条 originating_session_id 为本会话、from a、to b、reason quota、elapsed 120 的 FailoverEvent
  When 转发器处理该事件
  Then events.jsonl 新增一行 kind 为 fallback_switch
  And 该行 session 等于本会话 id、model_lane 等于 b、detail 等于 "router failover: a -> b (quota, 120ms)"

Scenario: gateway 路径他会话与 None 不写事件
  Test: obs_fallback_switch_gateway_ignores_other_and_none
  Given 转发器收到一条 originating_session_id 为其它会话与一条为 None 的 FailoverEvent
  When 转发器处理两条事件
  Then events.jsonl 不新增任何行

Scenario: debounce 抑制通知但不抑制事件
  Test: obs_fallback_switch_written_even_when_notice_debounced
  Given 转发器在 debounce 窗口内连续收到两条本会话 FailoverEvent
  When 转发器处理两条事件
  Then events.jsonl 新增两行 fallback_switch
  And 客户端通知只发送一条

Scenario: 事件写失败不阻断通知
  Test: obs_fallback_switch_write_failure_does_not_block_notice
  Given data_dir 指向一个不可写路径
  When 转发器处理一条本会话 FailoverEvent
  Then 客户端通知照常发送
  And 转发器未退出

Scenario: serve/UI 路径本会话 failover 写入事件(critical)
  Tags: critical
  Test: obs_fallback_switch_ui_forwarder_writes_own_session
  Given spawn_router_failover_forwarder 以临时 ledger data_dir 启动且广播一条本会话 FailoverEvent
  When 转发器处理该事件
  Then events.jsonl 新增一行 kind 为 fallback_switch 且 session 等于本会话 id

Scenario: serve/UI 路径他会话不写事件
  Test: obs_fallback_switch_ui_forwarder_ignores_other_session
  Given spawn_router_failover_forwarder 收到一条他会话的 FailoverEvent
  When 转发器处理该事件
  Then events.jsonl 不新增任何行

Scenario: 自纠预算耗尽的错误以 marker 开头且直接返回
  Test:
    Package: octos-agent
    Filter: malformed_exhaustion_error_carries_marker
  Given 模型连续四次返回 MalformedArgs
  When loop_runner 处理该 turn
  Then 返回的错误文本以 MALFORMED_TOOLCALL_EXHAUSTED_MARKER 开头并含 "feedback_limit=3 observed_malformed=4"
  And 该 turn 未进入重试分派

Scenario: 耗尽时 CLI 终态路径发射 malformed_exhausted(critical)
  Tags: critical
  Test: obs_malformed_exhausted_event_on_errored_terminal
  Given agent 连续四次 MalformedArgs 后 turn 以 Errored 终态结束
  When CLI 处理该终态
  Then events.jsonl 新增恰一行 kind 为 malformed_exhausted
  And 该行 detail 等于 "feedback_limit=3 observed_malformed=4" 且 session 等于该 turn 的会话
  And events.jsonl 中 turn_error 行数与处理前相等

Scenario: 正文中间含 marker 的普通错误不触发
  Test: obs_no_malformed_exhausted_when_marker_not_prefix
  Given turn 以一个正文中间含 MARKER 字样但不以其开头的普通错误结束
  When CLI 处理该终态
  Then events.jsonl 不新增 malformed_exhausted 行

Scenario: 事件流文档列出新 kind
  Test: obs_events_doc_lists_new_kinds
  Given 仓库检出
  When 读取 crates/octos-cli/src/obs_events.rs 的顶部注释
  Then 注释含 fallback_switch 与 malformed_exhausted

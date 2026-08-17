spec: task
name: "turn 退出时把未消费的 steer 输入返还客户端"
tags: [ui-protocol, steer, turn-lifecycle, octos-cli, octos-core]
estimate: 1d
---

## Intent

`turn/steer` 返回 `steered:true` 只表示输入进入了 pending-input buffer；如果 turn
在下一次 drain 之前被中断（事故 2026-08-17：steer 受理 32 秒后用户按 Esc，agent
被 abort），`run_standalone_turn` 在 `agent_task.await` 之后只把残留 drain 出来打一条
WARN，客户端因 RPC 已成功而认为文本"已发送"，文本静默丢失。本任务把这次 drop
变成协议事件：残留输入以新的 `turn/steer_dropped` 通知按原顺序返还客户端并写入
ledger（可 replay），使客户端能确定性地重新入队，而不是靠"没收到 echo"推断。

## Decisions

<!-- lint-ack: verification-metadata-suggestion — 两个错误路径场景使用进程内 stdio WsConnection + 内存 ledger fixture，无真实外部 I/O -->

- 新增 `octos-core` 通知 `UiNotification::TurnSteerDropped(TurnSteerDroppedEvent)`，
  method 常量 `methods::TURN_STEER_DROPPED = "turn/steer_dropped"`；事件字段
  `session_id`、`topic`（可选，与其他 turn 事件同样走 `set_topic_if_absent`）、
  `turn_id`、`inputs: Vec<String>`（保持 buffer 顺序）、`reason: String`。
- `reason` 取值：turn 被中断（`interrupt_observed`）时为 `"interrupted"`，其余为
  `"turn_ended"`。
- 分层：不依赖 `api` feature 的纯函数
  `steer_return::leftover_steer_notification(session_id, turn_id, leftovers,
  interrupt_observed) -> Option<UiNotification>`（空残留返回 `None`）负责事件形状；
  `api` 侧的 `settle_leftover_steers(buffer, ws, ledger, session_id, turn_id,
  interrupt_observed) -> usize` 在 `run_standalone_turn` 中 `agent_task.await`
  之后的现有残留 drain 处调用它并发送；有残留才发送，返回返还条数；原 WARN
  日志保留。
- 验证边界说明：`octos-cli` 的 `api` 模块（`WsConnection`、ledger）在 `api`
  feature 后面，`agent-spec` 的默认 `cargo test -p octos-cli` 不启用它；因此形状/
  顺序/reason/空残留由纯函数测试机械验证，发送与 ledger 行为由 `--features api`
  下的测试验证并以 `Review: human` 场景登记。
- 发送方式：`send_notification_durable`（等待容量、写 ledger），因为载荷是用户
  文本，不允许因背压丢帧；该通知在 turn 终态事件之后发出，客户端应把它当作
  "终态后需重新入队的输入"处理。
- 服务端在 `Interrupting` 状态下继续受理 steer 的行为不变——被受理的输入现在保证
  要么被 drain 进对话，要么通过 `turn/steer_dropped` 返还。
- 不修改 `TurnErrorEvent`/`TurnCompletedEvent` 的字段，保持既有客户端兼容（由
  "没有残留时不发任何帧"场景与既有终态测试共同约束：终态帧形状不变）。

## Boundaries

### Allowed Changes
- crates/octos-core/src/ui_protocol.rs
- crates/octos-core/src/ui_protocol_tests.rs
- crates/octos-cli/src/lib.rs
- crates/octos-cli/src/steer_return.rs
- crates/octos-cli/src/api/ui_protocol.rs
- crates/octos-cli/src/api/ui_protocol_ledger.rs
- crates/octos-cli/src/api/ui_protocol_tests.rs
- specs/task-return-unconsumed-steer-inputs.spec.md

### Forbidden
- 不改变 `turn/steer` 的受理判定（`TurnSteerDecision`）与返回值。
- 不改变 turn 终态事件（`turn/error`、`turn/completed`）的载荷。
- 不在残留 drain 处直接把输入重新注入新的 turn（是否重发由客户端决定）。
- 不新增 crate 依赖。

## Completion Criteria

### Rule: leftover-steers-returned — 残留 steer 以协议事件返还
Scenario: 中断后残留的 steer 按顺序生成 turn/steer_dropped 事件（critical）
  Tags: critical
  Test:
    Package: octos-cli
    Filter: leftover_steer_notification_preserves_order_and_labels_interrupted
  Given 两条已受理但未 drain 的输入
  When `leftover_steer_notification` 以 `interrupt_observed = true` 被调用
  Then 返回 `TurnSteerDropped` 事件，`inputs` 与输入顺序一致
  And `reason` 为 `"interrupted"` 且 `session_id`/`turn_id` 正确
  And 事件的 method 为 `turn/steer_dropped`

Scenario: 正常结束时的残留标为 turn_ended
  Test:
    Package: octos-cli
    Filter: leftover_steer_notification_labels_turn_ended_without_interrupt
  Given 一条残留输入
  When `leftover_steer_notification` 以 `interrupt_observed = false` 被调用
  Then 事件的 `reason` 为 `"turn_ended"`

Scenario: 没有残留时不生成事件（错误路径：不能产生空返还）
  Test:
    Package: octos-cli
    Filter: no_leftover_steers_produce_no_notification
  Given 空的残留列表
  When `leftover_steer_notification` 被调用
  Then 返回 `None`
  And `TurnErrorEvent`/`TurnCompletedEvent` 的字段集不因本任务改变

Scenario: api 侧把事件 durable 发送并写入 ledger、buffer 被清空（需 --features api）
  Review: human
  Test:
    Package: octos-cli
    Filter: leftover_steers_at_turn_end_are_returned_as_turn_steer_dropped
  Given 一个 `SteerBuffer` 里有两条残留输入与一个 stdio `WsConnection`
  When `settle_leftover_steers` 以 `interrupt_observed = true` 被调用
  Then 连接上收到一帧 `turn/steer_dropped` 且 ledger 追加了同一条通知
  And 返回值为 2 且 buffer 之后为空

Scenario: 连接已失效时残留仍写入 ledger（错误路径：断连不丢文本；需 --features api）
  Review: human
  Test:
    Package: octos-cli
    Filter: leftover_steers_are_ledgered_even_when_connection_write_fails
  Given 连接的 stdio writer 已经关闭
  When `settle_leftover_steers` 处理一条残留输入
  Then ledger 仍追加了 `turn/steer_dropped` 通知以供 reconnect replay
  And 返回值为 1

### Rule: protocol-shape — 事件形状与路由
Scenario: 事件通过 method 与 params 往返编码
  Test:
    Package: octos-core
    Filter: turn_steer_dropped_round_trips_through_method_and_params
  Given 一个 `TurnSteerDroppedEvent`
  When 以 `method_name()` + `to_params()` 编码再用 `from_method_params` 解码
  Then method 为 `turn/steer_dropped` 且解码结果与原事件相等

Scenario: 事件的 topic 路由与其他 turn 事件一致
  Test:
    Package: octos-core
    Filter: turn_steer_dropped_topic_routing_matches_other_turn_events
  Given 一个 `session_id` 带 topic 后缀而 `topic` 字段为空的事件
  When 读取 `topic()` 并调用 `set_topic_if_absent`
  Then `topic()` 返回 session key 的 topic
  And 已有 topic 不会被覆盖

## Out of Scope

- 客户端（octoscode）消费 `turn/steer_dropped` 并重新入队（需先把 octos-core 重新 pin 到本改动之后的 rev）。
- interrupt/steer 的 INFO 级关联日志（F7）、`octos serve` fd 累积（F8）。
- 把残留输入合并进 `turn/error`/`turn/completed` 载荷。

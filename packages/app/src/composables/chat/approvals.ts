import { readonly, ref } from "vue";
import { chatApi } from "./api";
import type {
  AgentEvent,
  ApprovalDecision,
  PendingApproval,
  PendingApprovalDto,
} from "./display";

export function createApprovals() {
  let disposed = false;
  // --- 审批状态 ---------------------------------------------------------------
  // 按会话分桶的 pending 审批：事件（upsert/remove）与 pending 快照端点
  // （整表替换）两路维护；重放幂等由「requested 后必跟 resolved」的事件
  // 时序保证。审批内存即焚，不进消息 reducer。

  /** 会话 id → 待决审批列表（审批卡的数据源）。 */
  const pendingApprovals = ref(new Map<string, PendingApproval[]>());
  const approvalVersions = new Map<string, number>();
  let approvalSnapshotGeneration = 0;
  const approvalLoads = new Map<string, number>();

  /** approval_requested 事件负载 → 展示形状。 */
  function approvalFromEvent(
    data: Extract<AgentEvent, { type: "approval_requested" }>["data"],
  ): PendingApproval {
    return {
      approvalId: data.approval_id,
      agentId: data.agent_id ?? null,
      callId: data.call_id,
      tool: data.tool,
      kind: data.kind,
      title: data.title,
      path: data.path ?? null,
      command: data.command ?? null,
      outside: data.outside === true,
      ruleSuggestion: data.rule_suggestion ?? null,
    };
  }

  /** pending 快照 DTO → 展示形状（与事件负载同字段，另带 run/created_at）。 */
  function approvalFromDto(dto: PendingApprovalDto): PendingApproval {
    return {
      approvalId: dto.approval_id,
      agentId: dto.agent_id ?? null,
      callId: dto.call_id,
      tool: dto.tool,
      kind: dto.kind,
      title: dto.title,
      path: dto.path ?? null,
      command: dto.command ?? null,
      outside: dto.outside === true,
      ruleSuggestion: dto.rule_suggestion ?? null,
    };
  }

  /** 按 approvalId upsert（重放重复到达时原地覆盖，不产生重复卡片）。 */
  function upsertApproval(conversationId: string, approval: PendingApproval) {
    approvalVersions.set(
      conversationId,
      (approvalVersions.get(conversationId) ?? 0) + 1,
    );
    const list = pendingApprovals.value.get(conversationId) ?? [];
    const index = list.findIndex(
      (item) => item.approvalId === approval.approvalId,
    );
    const next =
      index === -1 ? [...list, approval] : list.with(index, approval);
    pendingApprovals.value = new Map(pendingApprovals.value).set(
      conversationId,
      next,
    );
  }

  /** 移除一条 pending（resolved 事件 / decide 落地；未命中时静默幂等）。 */
  function removeApproval(conversationId: string, approvalId: string) {
    approvalVersions.set(
      conversationId,
      (approvalVersions.get(conversationId) ?? 0) + 1,
    );
    const list = pendingApprovals.value.get(conversationId);
    if (!list?.some((item) => item.approvalId === approvalId)) return;
    pendingApprovals.value = new Map(pendingApprovals.value).set(
      conversationId,
      list.filter((item) => item.approvalId !== approvalId),
    );
  }

  /**
   * 拉取会话的 pending 快照并以 GET 为准整表替换该会话的分桶（页面刷新/
   * 重连后的真相源；事件重放在其之后叠加）。尽力而为：失败（网络/非 2xx）
   * 保留旧列表静默返回，绝不阻断 attachRun 的重连主流程。
   */
  const refreshApprovals = async (conversationId: string) => {
    const generation = ++approvalSnapshotGeneration;
    const version = approvalVersions.get(conversationId) ?? 0;
    approvalLoads.set(conversationId, generation);
    try {
      const { data, error } = await chatApi.approvals(conversationId);
      if (
        !disposed &&
        !error &&
        data &&
        approvalLoads.get(conversationId) === generation &&
        (approvalVersions.get(conversationId) ?? 0) === version
      ) {
        pendingApprovals.value = new Map(pendingApprovals.value).set(
          conversationId,
          data.map(approvalFromDto),
        );
      }
    } catch {
      // 网络层异常：与 HTTP 错误同路径，保留旧列表
    }
  };

  /**
   * 应答一条审批：POST decide；成功（或 404=已决/未知）即本地清掉
   * 条目——approval_resolved 事件随后到达时的移除是幂等空操作。其余错误
   * 保留卡片供重试。
   */
  const decideApproval = async (
    conversationId: string,
    approvalId: string,
    decision: ApprovalDecision,
    pattern?: string,
  ) => {
    const { error, response } = await chatApi.decideApproval(
      conversationId,
      approvalId,
      decision,
      pattern,
    );
    // 404 = 已决/未知 id（decide 的幂等语义）：与成功同等对待，静默清卡。
    const settled = !error || response.status === 404;
    if (settled && !disposed) removeApproval(conversationId, approvalId);
    return settled;
  };

  function clear() {
    pendingApprovals.value = new Map();
    approvalLoads.clear();
    approvalVersions.clear();
  }
  function dispose() {
    disposed = true;
    approvalLoads.clear();
  }
  return {
    pendingApprovals: readonly(pendingApprovals),
    refreshApprovals,
    decideApproval,
    upsert: (
      id: string,
      data: Extract<AgentEvent, { type: "approval_requested" }>["data"],
    ) => upsertApproval(id, approvalFromEvent(data)),
    remove: removeApproval,
    clear,
    dispose,
  };
}

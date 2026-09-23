<template>
  <DialogRoot :open="visible" @update:open="handleUpdateOpen">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 z-50 bg-overlay" />
      <DialogContent
        class="fixed left-1/2 top-1/2 z-50 h-[650px] max-h-[calc(100vh-72px)] max-w-[calc(100vw-72px)] w-[940px] flex overflow-hidden border border-neutral-5 rounded-xl bg-panel shadow-[var(--oct-shadow)] -translate-x-1/2 -translate-y-1/2"
      >
        <!-- 左侧：搜索 + 分类导航（provider 设置是 AI 分类下的子项） -->
        <div
          class="w-[216px] flex flex-none flex-col border-r border-line-soft bg-titlebar"
        >
          <div class="flex-none p-3 pb-2">
            <!-- 配置作用域：全局或某个项目；provider 系配置只在全局存在 -->
            <AppSelect
              :model-value="scopeId"
              :options="projectScopeOptions"
              placeholder="选择作用域"
              title="配置作用域"
              trigger-class="w-full text-neutral-10"
              @update:model-value="selectScope"
            />
            <div
              class="mt-2 h-7 flex items-center gap-[7px] border border-line-soft rounded-md bg-surface px-[9px] text-faint transition-colors focus-within:border-accent-6"
            >
              <i-lucide-search class="h-3 w-3 flex-none" />
              <input
                v-model="query"
                placeholder="Search settings…"
                class="min-w-0 flex-1 border-none bg-transparent text-[12.5px] text-neutral-10 outline-none placeholder:text-faint"
              />
            </div>
          </div>
          <div class="flex-1 overflow-y-auto px-1.5 pb-3">
            <button
              class="h-7 w-full flex items-center gap-2 rounded-md px-2 text-left text-[12.5px] text-neutral-10 transition-colors"
              :class="view.name !== 'ai' ? 'bg-neutral-1 font-semibold' : ''"
              @click="view = { name: 'ai' }"
            >
              <i-lucide-chevron-right
                class="h-2.5 w-2.5 flex-none text-faint"
              />
              AI
            </button>
          </div>
        </div>

        <!-- 右侧：顶栏（面包屑）+ 内容 -->
        <div class="min-w-0 flex flex-1 flex-col">
          <div
            class="h-[46px] flex flex-none items-center gap-2 border-b border-line-soft px-3 pl-4"
          >
            <template v-if="view.name !== 'ai'">
              <button
                class="h-[22px] w-[22px] flex-none control-ghost rounded-md control-focus"
                title="返回"
                @click="goBack"
              >
                <i-lucide-arrow-left class="h-3.5 w-3.5" />
              </button>
              <button
                class="text-[12.5px] text-dim transition-colors hover:text-neutral-10"
                @click="view = { name: 'ai' }"
              >
                AI
              </button>
              <span class="text-[12.5px] text-faint">/</span>
              <template v-if="inProviderTree">
                <button
                  class="text-[12.5px] text-dim transition-colors hover:text-neutral-10"
                  @click="view = { name: 'providers' }"
                >
                  LLM Providers
                </button>
                <template v-if="view.name === 'provider'">
                  <span class="text-[12.5px] text-faint">/</span>
                  <span class="text-[12.5px] text-neutral-10 font-semibold">{{
                    selected?.name
                  }}</span>
                </template>
                <template v-else-if="view.name === 'connect-provider'">
                  <span class="text-[12.5px] text-faint">/</span>
                  <span class="text-[12.5px] text-neutral-10 font-semibold"
                    >Connect Provider</span
                  >
                </template>
              </template>
              <template v-else>
                <button
                  class="text-[12.5px] text-dim transition-colors hover:text-neutral-10"
                  @click="view = { name: 'mcp' }"
                >
                  MCP Servers
                </button>
                <template v-if="view.name === 'mcp-edit'">
                  <span class="text-[12.5px] text-faint">/</span>
                  <span class="text-[12.5px] text-neutral-10 font-semibold">{{
                    editingServer ? editingServer.name : "Add Server"
                  }}</span>
                </template>
              </template>
            </template>
            <span v-else class="text-[12.5px] text-neutral-10 font-semibold">
              AI
            </span>
            <span class="flex-1" />
            <button
              v-if="view.name === 'providers'"
              class="h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
              @click="startConnect"
            >
              <i-lucide-plus class="h-3 w-3" />
              Connect Provider
            </button>
            <button
              v-if="view.name === 'mcp'"
              class="h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
              @click="startAddServer"
            >
              <i-lucide-plus class="h-3 w-3" />
              Add Server
            </button>
            <DialogClose
              class="h-[26px] w-[26px] control-ghost rounded-md text-[12px] control-focus"
              title="关闭（Esc）"
            >
              ✕
            </DialogClose>
          </div>

          <div class="flex-1 overflow-y-auto px-7 pb-12 pt-[18px]">
            <!-- ============ AI 根页：按作用域列出配置入口 ============ -->
            <template v-if="view.name === 'ai'">
              <DialogTitle class="text-[16.5px] text-strong font-semibold">
                AI
              </DialogTitle>
              <p class="mt-1 text-[12px] text-dim">
                {{
                  inProjectScope
                    ? `项目「${activeScopeProjectName}」专属的配置入口；全局配置切到「全局」作用域查看。`
                    : "模型 Provider 与聊天标签页可用模型的配置入口。"
                }}
              </p>
              <div
                class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
              >
                General
              </div>
              <div
                v-if="!inProjectScope"
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-3"
              >
                <div class="min-w-0 flex-1">
                  <div class="flex items-center gap-2">
                    <span class="text-[13px] text-strong font-semibold">
                      LLM Providers
                    </span>
                    <span
                      class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                    >
                      全局
                    </span>
                  </div>
                  <div class="mt-0.5 text-[12px] text-dim">
                    连接模型 Provider，并选择聊天标签页中显示哪些模型。
                  </div>
                </div>
                <button
                  class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                  @click="view = { name: 'providers' }"
                >
                  Configure
                  <i-lucide-chevron-right class="h-3 w-3 text-dim" />
                </button>
              </div>
              <div
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-3"
              >
                <div class="min-w-0 flex-1">
                  <div class="text-[13px] text-strong font-semibold">
                    MCP Servers
                  </div>
                  <div class="mt-0.5 text-[12px] text-dim">
                    {{
                      inProjectScope
                        ? "仅该项目对话可用的 MCP servers；全局配置对所有项目生效。"
                        : "全局作用域的 MCP servers，对所有项目生效；项目专属的切到某个项目作用域添加。"
                    }}
                  </div>
                </div>
                <button
                  class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                  title="配置 MCP Servers"
                  @click="view = { name: 'mcp' }"
                >
                  Configure
                  <i-lucide-chevron-right class="h-3 w-3 text-dim" />
                </button>
              </div>
            </template>

            <!-- ============ 已连接的 Provider 列表 ============ -->
            <template v-else-if="view.name === 'providers'">
              <div
                v-for="provider in connectedProviders"
                :key="provider.id"
                class="border-b border-line-soft border-dashed py-3"
              >
                <div class="flex items-center gap-2">
                  <span
                    class="h-1.5 w-1.5 flex-none rounded-full bg-success-10"
                  />
                  <span class="text-[13px] text-strong font-semibold">{{
                    provider.name
                  }}</span>
                  <span
                    class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                  >
                    {{ provider.adapter_type }}
                  </span>
                  <span class="flex-1" />
                  <button
                    class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                    @click="view = { name: 'provider', id: provider.id }"
                  >
                    Configure
                    <i-lucide-chevron-right class="h-3 w-3 text-dim" />
                  </button>
                </div>
                <div class="mt-1.5 pl-3.5 text-[12px] text-dim">
                  已启用 {{ enabledCount(provider) }} /
                  {{ provider.models.length }} 个模型
                </div>
              </div>

              <div
                v-if="connectedProviders.length === 0 && !loading"
                class="py-10 text-center"
              >
                <p class="text-[12.5px] text-faint">尚未连接任何 Provider。</p>
                <button
                  class="mt-3 h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                  @click="startConnect"
                >
                  <i-lucide-plus class="h-3 w-3" />
                  Connect Provider
                </button>
              </div>
              <p
                v-if="loading"
                class="py-8 text-center text-[12.5px] text-faint"
              >
                Loading…
              </p>
            </template>

            <!-- ============ Connect Provider（选已有 / 手动添加） ============ -->
            <template v-else-if="view.name === 'connect-provider'">
              <div class="flex items-center gap-1">
                <button
                  class="h-[26px] rounded-md px-2.5 text-[12.5px] transition-colors"
                  :class="
                    connectMode === 'pick'
                      ? 'bg-neutral-2 text-strong font-semibold'
                      : 'text-dim hover:bg-neutral-1'
                  "
                  @click="connectMode = 'pick'"
                >
                  从列表连接
                </button>
                <button
                  class="h-[26px] rounded-md px-2.5 text-[12.5px] transition-colors"
                  :class="
                    connectMode === 'manual'
                      ? 'bg-neutral-2 text-strong font-semibold'
                      : 'text-dim hover:bg-neutral-1'
                  "
                  @click="connectMode = 'manual'"
                >
                  手动添加
                </button>
              </div>

              <!-- 模式一：从已有（未连接）Provider 中选择并填 Key -->
              <template v-if="connectMode === 'pick'">
                <div
                  class="mt-4 max-h-[260px] overflow-y-auto border border-line-soft rounded-lg bg-surface"
                >
                  <button
                    v-for="provider in unconnectedProviders"
                    :key="provider.id"
                    class="h-9 w-full flex items-center gap-2 border-b border-line-soft px-3 text-left text-[12.5px] transition-colors last:border-b-0"
                    :class="
                      pickId === provider.id
                        ? 'bg-accent-3 text-neutral-10'
                        : 'text-dim hover:bg-neutral-1 hover:text-neutral-10'
                    "
                    @click="pickId = provider.id"
                  >
                    <span class="truncate">{{ provider.name }}</span>
                    <span
                      class="ml-auto flex-none rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                    >
                      {{ provider.adapter_type }}
                    </span>
                  </button>
                  <p
                    v-if="unconnectedProviders.length === 0"
                    class="px-3 py-4 text-center text-[12px] text-faint"
                  >
                    所有内置 Provider 都已连接，可用「手动添加」接入新的。
                  </p>
                </div>

                <template v-if="picked">
                  <div
                    class="mt-5 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
                  >
                    Connect {{ picked.name }}
                  </div>
                  <div
                    class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-2.5"
                  >
                    <div class="min-w-0 flex-1">
                      <div class="text-[13px] text-strong font-semibold">
                        API Key
                      </div>
                      <div class="mt-0.5 text-[12px] text-dim">
                        填入 {{ picked.name }} 的 API Key，保存后即完成连接。
                      </div>
                    </div>
                    <input
                      v-model="pickKey"
                      :type="showPickKey ? 'text' : 'password'"
                      placeholder="Enter API key..."
                      class="h-[26px] w-[240px] flex-none control-surface px-2.5 text-[12px] control-focus"
                      @keydown.enter="connectPicked"
                    />
                    <button
                      class="h-[26px] flex-none control-ghost rounded-md px-2 text-[12px] control-focus"
                      @click="showPickKey = !showPickKey"
                    >
                      {{ showPickKey ? "Hide" : "Show" }}
                    </button>
                    <button
                      class="h-[26px] flex-none control-solid rounded-md px-3 text-[12px] font-medium control-focus transition-colors disabled:opacity-50"
                      :disabled="!pickKey.trim()"
                      @click="connectPicked"
                    >
                      Connect
                    </button>
                  </div>
                </template>
              </template>

              <!-- 模式二：手动添加（Anthropic / OpenAI 兼容） -->
              <template v-else>
                <div class="mt-4 flex items-center gap-1">
                  <button
                    v-for="t in PROVIDER_TYPES"
                    :key="t.type"
                    class="h-[26px] rounded-md px-2.5 text-[12.5px] transition-colors"
                    :class="
                      addForm.type === t.type
                        ? 'bg-neutral-2 text-strong font-semibold'
                        : 'text-dim hover:bg-neutral-1'
                    "
                    @click="switchType(t.type)"
                  >
                    {{ t.label }}
                  </button>
                </div>

                <div class="mt-4 space-y-4">
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      Provider Name
                    </span>
                    <span class="ml-1 text-[12px] text-danger-10">*</span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      用于识别该 Provider 的唯一名称。
                    </div>
                    <input
                      v-model="addForm.name"
                      :placeholder="
                        addForm.type === 'anthropic' ? 'Anthropic' : 'OpenAI'
                      "
                      class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
                    />
                  </label>
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      API URL
                    </span>
                    <span class="ml-1 text-[12px] text-danger-10">*</span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      兼容 API 的 base URL。
                    </div>
                    <input
                      v-model="addForm.base_url"
                      :placeholder="
                        addForm.type === 'anthropic'
                          ? 'https://api.anthropic.com/v1'
                          : 'https://api.openai.com/v1'
                      "
                      class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
                    />
                  </label>
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      API Key
                    </span>
                    <span class="ml-1 text-[12px] text-danger-10">*</span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      调用该 Provider 使用的密钥。
                    </div>
                    <input
                      v-model="addForm.api_key"
                      type="password"
                      placeholder="sk-..."
                      class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
                    />
                  </label>
                  <p v-if="addError" class="text-[12px] text-danger-10">
                    {{ addError }}
                  </p>
                  <div class="flex items-center gap-2">
                    <button
                      class="h-[28px] control-solid rounded-md px-4 text-[12.5px] font-medium control-focus transition-colors disabled:opacity-50"
                      :disabled="!canSubmitProvider || creating"
                      @click="submitProvider"
                    >
                      {{ creating ? "Creating…" : "Create Provider" }}
                    </button>
                    <button
                      class="h-[28px] control-ghost rounded-md px-3 text-[12.5px] control-focus"
                      @click="view = { name: 'providers' }"
                    >
                      Cancel
                    </button>
                  </div>
                </div>
              </template>
            </template>

            <!-- ============ Provider 详情（Key + 模型可见性） ============ -->
            <template v-else-if="view.name === 'provider' && selected">
              <div class="flex items-center gap-2">
                <DialogTitle class="text-[16.5px] text-strong font-semibold">
                  {{ selected.name }}
                </DialogTitle>
                <span
                  class="rounded bg-success-7 px-1.5 py-px text-[10.5px] text-success-contrast font-medium leading-[16px]"
                >
                  已连接
                </span>
              </div>
              <p class="mt-1 text-[12px] text-dim">
                <span class="font-mono">{{ selected.adapter_type }}</span>
                <template v-if="selected.base_url">
                  · <span class="font-mono">{{ selected.base_url }}</span>
                </template>
              </p>

              <div
                class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
              >
                Connection
              </div>
              <div
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-2.5"
              >
                <div class="min-w-0 flex-1">
                  <div class="text-[13px] text-strong font-semibold">
                    API Key
                  </div>
                  <div class="mt-0.5 text-[12px] text-dim">
                    已配置。输入新 Key 并保存即可替换。
                  </div>
                </div>
                <input
                  :type="showKeys[selected.id] ? 'text' : 'password'"
                  placeholder="••••••••（已配置）"
                  class="h-[26px] w-[240px] flex-none control-surface px-2.5 text-[12px] control-focus"
                  :value="keyInputs[selected.id] ?? ''"
                  @input="
                    (e: Event) => {
                      keyInputs[selected!.id] = (
                        e.target as HTMLInputElement
                      ).value;
                    }
                  "
                  @keydown.enter="saveKey(selected.id)"
                />
                <button
                  class="h-[26px] flex-none control-ghost rounded-md px-2 text-[12px] control-focus"
                  @click="showKeys[selected.id] = !showKeys[selected.id]"
                >
                  {{ showKeys[selected.id] ? "Hide" : "Show" }}
                </button>
                <button
                  class="h-[26px] flex-none control-solid rounded-md px-3 text-[12px] font-medium control-focus transition-colors disabled:opacity-50"
                  :disabled="!keyInputs[selected.id]?.trim()"
                  @click="saveKey(selected.id)"
                >
                  Save
                </button>
              </div>

              <div
                class="mt-6 border-b border-line border-dashed pb-1.5 text-[11px] text-dim tracking-[0.5px] font-mono"
              >
                Models
              </div>
              <div
                v-for="model in filteredModels"
                :key="model.model_id"
                class="flex items-center gap-[18px] border-b border-line-soft border-dashed py-2.5"
              >
                <div class="min-w-0 flex-1">
                  <div class="truncate text-[13px] text-strong font-semibold">
                    {{ model.name }}
                  </div>
                  <div class="mt-0.5 text-[11.5px] text-faint font-mono">
                    {{ model.model_id }}
                  </div>
                </div>
                <div class="flex flex-none items-center gap-1">
                  <span
                    v-if="model.limit_context"
                    class="rounded bg-neutral-2 px-1 py-px text-[10px] text-dim"
                  >
                    {{ formatTokens(model.limit_context) }}
                  </span>
                  <span
                    v-if="model.tool_call"
                    class="rounded bg-neutral-2 px-1 py-px text-[10px] text-dim"
                    >tools</span
                  >
                  <span
                    v-if="model.reasoning"
                    class="rounded bg-neutral-2 px-1 py-px text-[10px] text-dim"
                    >thinking</span
                  >
                </div>
                <button
                  class="relative h-[19px] w-[34px] flex-none rounded-full transition-colors"
                  :class="model.is_enabled ? 'bg-accent-7' : 'bg-neutral-2'"
                  role="switch"
                  :aria-checked="model.is_enabled"
                  :title="
                    model.is_enabled
                      ? '在模型选择器中隐藏'
                      : '在模型选择器中显示'
                  "
                  @click="
                    toggleModelEnabled(
                      selected.id,
                      model.model_id,
                      !model.is_enabled,
                    )
                  "
                >
                  <span
                    class="absolute left-[2px] top-[2px] h-[15px] w-[15px] rounded-full bg-white shadow-[0_1px_3px_rgba(0,0,0,0.3)] transition-transform"
                    :class="model.is_enabled ? 'translate-x-[15px]' : ''"
                  />
                </button>
              </div>
              <p
                v-if="filteredModels.length === 0"
                class="py-4 text-[12px] text-faint"
              >
                无匹配的模型
              </p>

              <!-- 自定义 Provider：手动登记模型 -->
              <div v-if="selected.source === 'custom'" class="mt-4">
                <div class="text-[12.5px] text-neutral-10 font-semibold">
                  Add Model
                </div>
                <div
                  class="mt-2 flex items-end gap-2 border border-line-soft rounded-lg bg-surface p-3"
                >
                  <label class="block min-w-0 flex-1">
                    <span class="text-[11px] text-dim">Model ID</span>
                    <input
                      v-model="modelForm.model_id"
                      placeholder="e.g. gpt-5"
                      class="mt-1 h-[26px] w-full control-surface px-2.5 text-[12px] control-focus"
                    />
                  </label>
                  <label class="block min-w-0 flex-1">
                    <span class="text-[11px] text-dim">Display Name</span>
                    <input
                      v-model="modelForm.name"
                      placeholder="e.g. GPT-5"
                      class="mt-1 h-[26px] w-full control-surface px-2.5 text-[12px] control-focus"
                    />
                  </label>
                  <label class="block w-[120px] flex-none">
                    <span class="text-[11px] text-dim">Context</span>
                    <input
                      v-model="modelForm.limit_context"
                      placeholder="128000"
                      class="mt-1 h-[26px] w-full control-surface px-2.5 text-[12px] control-focus"
                    />
                  </label>
                  <button
                    class="h-[26px] flex-none control-solid rounded-md px-3 text-[12px] font-medium control-focus disabled:opacity-50"
                    :disabled="!modelForm.model_id.trim()"
                    @click="submitModel(selected.id)"
                  >
                    Add
                  </button>
                </div>
              </div>
            </template>

            <!-- ============ MCP Servers 列表（按当前作用域过滤） ============ -->
            <template v-else-if="view.name === 'mcp'">
              <DialogTitle class="text-[16.5px] text-strong font-semibold">
                MCP Servers
              </DialogTitle>
              <p class="mt-1 text-[12px] text-dim">
                为 Agent 接入外部工具服务器（stdio /
                http）；启用后下一次对话开始时连接。{{
                  inProjectScope
                    ? `此处为项目「${activeScopeProjectName}」专属配置，全局配置见「全局」作用域。`
                    : "此处为全局作用域，对所有项目生效。"
                }}
              </p>

              <p v-if="mcpError" class="mt-3 text-[12px] text-danger-10">
                {{ mcpError }}
              </p>

              <!-- enabled=false 整行弱化（含状态点/徽标/开关），开关仍可用 -->
              <div
                v-for="server in filteredMcpServers"
                :key="server.id"
                class="border-b border-line-soft border-dashed py-3 transition-opacity"
                :class="server.enabled ? '' : 'opacity-60'"
              >
                <div class="flex items-center gap-2">
                  <span
                    class="h-1.5 w-1.5 flex-none rounded-full"
                    :class="mcpStatusDot(server.status)"
                    :title="mcpStatusText(server)"
                  />
                  <span class="text-[13px] text-strong font-semibold">{{
                    server.name
                  }}</span>
                  <span
                    class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                  >
                    {{ scopeLabel(server) }}
                  </span>
                  <span
                    class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                  >
                    {{ server.transport }}
                  </span>
                  <span class="flex-1" />
                  <button
                    class="relative h-[19px] w-[34px] flex-none rounded-full transition-colors"
                    :class="server.enabled ? 'bg-accent-7' : 'bg-neutral-2'"
                    role="switch"
                    :aria-checked="server.enabled"
                    :aria-label="`启用 ${server.name}`"
                    :title="server.enabled ? '禁用该 server' : '启用该 server'"
                    @click="toggleMcpEnabled(server)"
                  >
                    <span
                      class="absolute left-[2px] top-[2px] h-[15px] w-[15px] rounded-full bg-white shadow-[0_1px_3px_rgba(0,0,0,0.3)] transition-transform"
                      :class="server.enabled ? 'translate-x-[15px]' : ''"
                    />
                  </button>
                  <button
                    class="h-[26px] inline-flex flex-none items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                    :title="`配置 ${server.name}`"
                    @click="startEditServer(server)"
                  >
                    Configure
                    <i-lucide-chevron-right class="h-3 w-3 text-dim" />
                  </button>
                  <!-- 删除是破坏性操作，hover 反馈比普通 ghost（neutral-1，
                       亮色下 ~5% 黑底几乎不可见）强一档：提到 neutral-3，
                       与 Configure（control-surface）的 hover 同级，亮暗色
                       都可感知 -->
                  <button
                    class="h-[26px] w-[26px] flex-none rounded-md text-dim control-focus transition-colors active:bg-neutral-4 hover:bg-neutral-3 hover:text-neutral-10"
                    :title="`删除 ${server.name}`"
                    @click="removeMcpServer(server)"
                  >
                    <i-lucide-trash-2 class="h-3.5 w-3.5" />
                  </button>
                </div>
                <!-- 状态/错误副文本独占一行（长错误截断，完整文本悬停状态点
                     的 title 可见）；stdio 行显示 command、http 行显示
                     url，用 text-dim 保证亮色下可读（text-faint 在亮色
                     面板上对比度不足） -->
                <div class="mt-1.5 pl-3.5">
                  <span
                    class="block truncate text-[12px]"
                    :class="mcpStatusClass(server.status)"
                  >
                    {{ mcpStatusText(server) }}
                  </span>
                  <span
                    v-if="server.command || server.url"
                    class="mt-0.5 block truncate text-[11.5px] text-dim font-mono"
                    :title="server.command ?? server.url ?? ''"
                  >
                    {{ server.command ?? server.url }}
                  </span>
                </div>
              </div>

              <div
                v-if="filteredMcpServers.length === 0 && !mcpLoading"
                class="py-10 text-center"
              >
                <p class="text-[12.5px] text-faint">{{ mcpEmptyText }}</p>
                <button
                  class="mt-3 h-[26px] inline-flex items-center gap-1.5 control-surface rounded-md px-2.5 text-[12px] font-medium control-focus"
                  @click="startAddServer"
                >
                  <i-lucide-plus class="h-3 w-3" />
                  Add Server
                </button>
              </div>
              <p
                v-if="mcpLoading"
                class="py-8 text-center text-[12.5px] text-faint"
              >
                Loading…
              </p>
            </template>

            <!-- ============ MCP Server 新增 / 编辑表单 ============ -->
            <template v-else-if="view.name === 'mcp-edit'">
              <div class="flex items-center gap-2">
                <DialogTitle class="text-[16.5px] text-strong font-semibold">
                  {{ editingServer ? "Edit MCP Server" : "Add MCP Server" }}
                </DialogTitle>
                <span
                  class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                >
                  {{ mcpForm.transport }}
                </span>
              </div>
              <p class="mt-1 text-[12px] text-dim">
                {{ mcpFormIntro }}
              </p>

              <div class="mt-4 max-w-[520px] space-y-4">
                <label class="block">
                  <span class="text-[13px] text-strong font-semibold">
                    Name
                  </span>
                  <span class="ml-1 text-[12px] text-danger-10">*</span>
                  <div class="mt-0.5 text-[12px] text-dim">
                    唯一名称，用于识别该 server。
                  </div>
                  <input
                    v-model="mcpForm.name"
                    placeholder="e.g. filesystem"
                    class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] control-focus"
                  />
                </label>
                <label class="block">
                  <span class="text-[13px] text-strong font-semibold">
                    Transport
                  </span>
                  <div class="mt-0.5 text-[12px] text-dim">
                    stdio 本地子进程；http 远程 Streamable HTTP 端点。
                  </div>
                  <div class="mt-2">
                    <AppSelect
                      :model-value="mcpForm.transport"
                      :options="mcpTransportOptions"
                      title="transport"
                      trigger-class="w-full text-neutral-10"
                      @update:model-value="selectMcpTransport"
                    />
                  </div>
                </label>
                <!-- stdio 专属字段：本地进程三件套 -->
                <template v-if="mcpForm.transport === 'stdio'">
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      Command
                    </span>
                    <span class="ml-1 text-[12px] text-danger-10">*</span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      启动命令（可执行文件）。
                    </div>
                    <input
                      v-model="mcpForm.command"
                      placeholder="e.g. npx"
                      class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] font-mono control-focus"
                    />
                  </label>
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      Args
                    </span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      命令行参数，每行一个。
                    </div>
                    <textarea
                      v-model="mcpForm.args"
                      rows="3"
                      placeholder="-y&#10;@modelcontextprotocol/server-filesystem&#10;/tmp"
                      class="mt-2 block w-full resize-y border border-neutral-5 rounded-md bg-surface px-3 py-2 text-[12.5px] text-neutral-10 font-mono control-focus outline-none transition-colors placeholder:text-faint"
                    />
                  </label>
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      Env
                    </span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      环境变量，每行一个 KEY=value。
                    </div>
                    <textarea
                      v-model="mcpForm.env"
                      rows="2"
                      placeholder="API_TOKEN=secret"
                      class="mt-2 block w-full resize-y border border-neutral-5 rounded-md bg-surface px-3 py-2 text-[12.5px] text-neutral-10 font-mono control-focus outline-none transition-colors placeholder:text-faint"
                    />
                  </label>
                </template>
                <!-- http 专属字段：端点 + 请求头（bearer 认证入口） -->
                <template v-else>
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      URL
                    </span>
                    <span class="ml-1 text-[12px] text-danger-10">*</span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      Streamable HTTP 端点（http:// 或 https:// 开头）。
                    </div>
                    <input
                      v-model="mcpForm.url"
                      placeholder="https://example.com/mcp"
                      class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] font-mono control-focus"
                    />
                  </label>
                  <label class="block">
                    <span class="text-[13px] text-strong font-semibold">
                      Headers
                    </span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      请求头，每行一个 KEY=value；Bearer 认证写
                      Authorization=Bearer &lt;token&gt;。
                    </div>
                    <textarea
                      v-model="mcpForm.headers"
                      rows="2"
                      placeholder="Authorization=Bearer sk-..."
                      class="mt-2 block w-full resize-y border border-neutral-5 rounded-md bg-surface px-3 py-2 text-[12.5px] text-neutral-10 font-mono control-focus outline-none transition-colors placeholder:text-faint"
                    />
                  </label>
                  <!-- OAuth 登录区：仅已保存的 http 行（start 需要服务端行与
                       落库的 url；新建表单先保存再回来登录）。 -->
                  <div
                    v-if="editingServer"
                    class="border border-line-soft rounded-lg bg-surface p-3"
                  >
                    <div class="flex items-center gap-2">
                      <span class="text-[13px] text-strong font-semibold">
                        OAuth
                      </span>
                      <span
                        v-if="oauthState?.kind === 'authorized'"
                        class="rounded bg-success-7 px-1.5 py-px text-[10.5px] text-success-contrast font-medium leading-[16px]"
                        :title="
                          oauthState.expiresAt
                            ? `有效期至 ${formatOauthExpiry(oauthState.expiresAt)}`
                            : '长期有效'
                        "
                      >
                        已授权
                      </span>
                      <span
                        v-else-if="oauthState?.kind === 'expired'"
                        class="rounded bg-danger-7 px-1.5 py-px text-[10.5px] text-danger-10 font-medium leading-[16px]"
                      >
                        需重新登录
                      </span>
                      <span
                        v-else
                        class="rounded bg-neutral-2 px-1.5 py-px text-[10.5px] text-dim font-medium leading-[16px]"
                      >
                        未授权
                      </span>
                    </div>
                    <div class="mt-0.5 text-[12px] text-dim">
                      远端服务器需要 OAuth 2.0 时在此登录（授权码 + PKCE，
                      基于已保存的 URL 发起）；已登录的 Bearer 优先于 Headers
                      里的静态 Authorization。
                      <template v-if="oauthState?.kind === 'authorized'">
                        {{
                          oauthState.expiresAt
                            ? `有效期至 ${formatOauthExpiry(oauthState.expiresAt)}`
                            : "长期有效"
                        }}
                        {{
                          editingServer?.oauth?.has_refresh_token
                            ? "· 支持自动刷新"
                            : ""
                        }}
                      </template>
                    </div>
                    <div class="mt-2 flex items-center gap-2">
                      <button
                        class="h-[26px] inline-flex items-center gap-1.5 control-solid rounded-md px-3 text-[12px] font-medium control-focus transition-colors disabled:opacity-50"
                        :disabled="oauthBusy"
                        :title="
                          oauthState?.kind === 'absent'
                            ? '登录并授权该服务器'
                            : '重新登录（更换凭据）'
                        "
                        @click="loginOauth"
                      >
                        {{
                          oauthWaiting
                            ? "等待授权完成…"
                            : oauthState?.kind === "absent"
                              ? "登录"
                              : "重新登录"
                        }}
                      </button>
                      <button
                        v-if="oauthState && oauthState.kind !== 'absent'"
                        class="h-[26px] inline-flex items-center control-ghost rounded-md px-2.5 text-[12px] control-focus transition-colors disabled:opacity-50"
                        :disabled="oauthBusy"
                        @click="logoutOauthLogin"
                      >
                        退出登录
                      </button>
                    </div>
                    <p
                      v-if="oauthError"
                      class="mt-1.5 text-[12px] text-danger-10"
                    >
                      {{ oauthError }}
                    </p>
                  </div>
                </template>
                <div class="flex items-start gap-4">
                  <label class="block w-[180px] flex-none">
                    <span class="text-[13px] text-strong font-semibold">
                      Tool Timeout
                    </span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      工具调用超时（毫秒），留空用默认值。
                    </div>
                    <input
                      v-model="mcpForm.toolTimeout"
                      placeholder="30000"
                      inputmode="numeric"
                      class="mt-2 h-[30px] w-full control-surface px-3 text-[13px] font-mono control-focus"
                    />
                  </label>
                  <label class="mt-[1px] min-w-0 flex-1">
                    <span
                      class="mt-[11px] text-[13px] text-strong font-semibold"
                    >
                      Project Scope
                    </span>
                    <div class="mt-0.5 text-[12px] text-dim">
                      全局对所有项目生效；项目内仅该项目的对话可见。
                    </div>
                    <div class="mt-2">
                      <AppSelect
                        :model-value="mcpForm.projectId"
                        :options="projectScopeOptions"
                        placeholder="全局"
                        title="作用域"
                        trigger-class="w-full text-neutral-10"
                        @update:model-value="selectMcpScope"
                      />
                    </div>
                  </label>
                </div>
                <!-- 只显示表单自己的错误；fetch/列表级错误（mcpError）留在
                     列表视图顶部，不经 ?? 泄漏进表单 -->
                <p v-if="mcpFormError" class="text-[12px] text-danger-10">
                  {{ mcpFormError }}
                </p>
                <div class="flex items-center gap-2">
                  <button
                    class="h-[28px] control-solid rounded-md px-4 text-[12.5px] font-medium control-focus transition-colors disabled:opacity-50"
                    :disabled="!canSubmitMcp || mcpSubmitting"
                    @click="submitMcpServer"
                  >
                    {{
                      mcpSubmitting
                        ? "Saving…"
                        : editingServer
                          ? "Save Changes"
                          : "Create Server"
                    }}
                  </button>
                  <button
                    class="h-[28px] control-ghost rounded-md px-3 text-[12.5px] control-focus"
                    @click="view = { name: 'mcp' }"
                  >
                    Cancel
                  </button>
                </div>
              </div>
            </template>
          </div>
        </div>
        <DialogDescription class="sr-only">
          连接 Provider 并选择聊天标签页中显示的模型。
        </DialogDescription>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>

<script setup lang="ts">
import {
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from "reka-ui";
import { computed, ref, watch } from "vue";
import AppSelect from "@/components/ui/AppSelect.vue";
import { mcpOauthState, useMcpServers } from "@/composables/useMcpServers";
import {
  mcpStatusClass,
  mcpStatusDot,
  mcpStatusText,
} from "@/composables/useMcpStatus";
import { useProjects } from "@/composables/useProjects";
import { useProviders } from "@/composables/useProviders";
import type { components } from "@/api/schema";

type ProviderResponse = components["schemas"]["ProviderResponse"];
type McpServerResponse = components["schemas"]["McpServerResponse"];

const props = defineProps<{
  visible: boolean;
  /** 工作区当前活动项目；作为项目作用域的默认选中项。 */
  activeProjectId: string | null;
  /**
   * 非空 = 打开时直接定位到该 MCP server 的编辑表单（状态栏菜单的
   * 「配置」深链）。宿主在弹窗关闭时应清空，避免普通打开误触发定位。
   */
  focusServerId?: string | null;
}>();

const emit = defineEmits<{
  close: [];
}>();

const {
  providers,
  loading,
  updateApiKey,
  toggleModelEnabled,
  createProvider,
  createModel,
} = useProviders();

const {
  servers: mcpServers,
  loading: mcpLoading,
  error: mcpError,
  fetchServers: fetchMcpServers,
  createServer,
  updateServer,
  deleteServer,
  toggleEnabled: toggleMcpServerEnabled,
  clearError: clearMcpError,
  startOauthLogin: startMcpOauth,
  waitForOauth: waitForMcpOauth,
  logoutOauth: logoutMcpOauth,
} = useMcpServers();

const { projects } = useProjects();

type View =
  | { name: "ai" }
  | { name: "providers" }
  | { name: "provider"; id: string }
  | { name: "connect-provider" }
  | { name: "mcp" }
  | { name: "mcp-edit"; id: string | null };

const view = ref<View>({ name: "ai" });
const query = ref("");
const keyInputs = ref({} as Record<string, string>);
const showKeys = ref({} as Record<string, boolean>);

/* ---------- 配置作用域：单个 select，值为哨兵（全局）或项目 id。 ---------- */
/* ---------- provider 系配置只在全局有意义，项目作用域仅暴露 MCP； ---------- */
/* ---------- MCP 按 project_id 归属分区展示。 ---------- */

/** AppSelect 的 value 是 string；用哨兵值表达"全局"（project_id 为 null）。 */
const GLOBAL_SCOPE = "__global__";

/** 当前配置作用域：GLOBAL_SCOPE 哨兵（全局）或项目 id。 */
const scopeId = ref<string>(GLOBAL_SCOPE);

const inProjectScope = computed(() => scopeId.value !== GLOBAL_SCOPE);

// 所选项目失效（列表加载/变更、项目被他处删除）时回退：活动项目 > 第一个 > 全局
watch(
  [projects, () => props.activeProjectId],
  () => {
    if (scopeId.value === GLOBAL_SCOPE) return;
    const list = projects.value;
    if (list.some((p) => p.id === scopeId.value)) return;
    const byActive =
      props.activeProjectId && list.some((p) => p.id === props.activeProjectId)
        ? props.activeProjectId
        : null;
    scopeId.value = byActive ?? list[0]?.id ?? GLOBAL_SCOPE;
  },
  { immediate: true },
);

const activeScopeProjectName = computed(
  () => projects.value.find((p) => p.id === scopeId.value)?.name ?? "",
);

function selectScope(value: string) {
  if (value === scopeId.value) return;
  scopeId.value = value;
  const name = view.value.name;
  if (
    name === "providers" ||
    name === "provider" ||
    name === "connect-provider"
  ) {
    // provider 系视图只在全局作用域存在
    view.value = { name: "ai" };
  } else if (name === "mcp-edit") {
    // 半截表单不跨作用域携带，回到目标作用域的列表
    view.value = { name: "mcp" };
  }
  // ai / mcp 视图跨作用域有效，原地保留（列表按新作用域响应式重过滤）
}

/* ---------- Connect Provider：从列表连接 / 手动添加 ---------- */
const connectMode = ref<"pick" | "manual">("pick");
const pickId = ref<string | null>(null);
const pickKey = ref("");
const showPickKey = ref(false);
const PROVIDER_TYPES = [
  { type: "anthropic", label: "Anthropic" },
  { type: "openai", label: "OpenAI 兼容" },
] as const;
const TYPE_DEFAULT_URLS = {
  anthropic: "https://api.anthropic.com/v1",
  openai: "https://api.openai.com/v1",
} as const;
const addForm = ref({
  type: "openai" as (typeof PROVIDER_TYPES)[number]["type"],
  name: "",
  base_url: "",
  api_key: "",
});
const addError = ref<string | null>(null);
const creating = ref(false);
const modelForm = ref({ model_id: "", name: "", limit_context: "" });

const selected = computed(
  () =>
    providers.value.find(
      (provider) =>
        provider.id === (view.value.name === "provider" ? view.value.id : null),
    ) ?? null,
);

// 详情页对应的 Provider 不存在时退回列表
watch(selected, (value) => {
  if (view.value.name === "provider" && !value) {
    view.value = { name: "providers" };
  }
});

const connectedProviders = computed(() => {
  const q = query.value.trim().toLowerCase();
  return providers.value
    .filter(
      (provider) =>
        provider.api_key_set && (!q || provider.name.toLowerCase().includes(q)),
    )
    .sort((a, b) => a.name.localeCompare(b.name));
});

const unconnectedProviders = computed(() => {
  const q = query.value.trim().toLowerCase();
  return providers.value
    .filter(
      (provider) =>
        !provider.api_key_set &&
        (!q || provider.name.toLowerCase().includes(q)),
    )
    .sort((a, b) => a.name.localeCompare(b.name));
});

const picked = computed(
  () =>
    unconnectedProviders.value.find(
      (provider) => provider.id === pickId.value,
    ) ?? null,
);

const filteredModels = computed(() => {
  const provider = selected.value;
  if (!provider) return [];
  const q = query.value.trim().toLowerCase();
  if (!q) return provider.models;
  return provider.models.filter(
    (model) =>
      model.name.toLowerCase().includes(q) ||
      model.model_id.toLowerCase().includes(q),
  );
});

// 面包屑子树：providers 系视图共用 "AI / LLM Providers" 前缀，mcp 系另走一支
const inProviderTree = computed(() =>
  ["providers", "provider", "connect-provider"].includes(view.value.name),
);

function goBack() {
  if (view.value.name === "ai") {
    view.value = { name: "ai" };
  } else if (view.value.name === "mcp" || view.value.name === "mcp-edit") {
    view.value = { name: "mcp" };
  } else {
    view.value = { name: "providers" };
  }
}

/* ---------- MCP Servers：列表 / 新增 / 编辑 ---------- */

const mcpForm = ref(emptyMcpForm());
const mcpFormError = ref<string | null>(null);
const mcpSubmitting = ref(false);
// OAuth（http 行编辑表单）的局部状态；定义在此因下面的 watch 在离开
// mcp-edit 视图时要清 oauthError（登录/等待/处理函数在文件后段）。
const oauthBusy = ref(false);
const oauthWaiting = ref(false);
const oauthError = ref<string | null>(null);

function emptyMcpForm() {
  return {
    name: "",
    /** "stdio" | "http"，决定表单展示哪组字段、提交带哪些键 */
    transport: "stdio",
    command: "",
    /** 一行一个命令行参数 */
    args: "",
    /** 一行一个 KEY=value */
    env: "",
    /** http 端点 */
    url: "",
    /** 一行一个 KEY=value（http 请求头，Bearer 认证入口） */
    headers: "",
    /** 空 = 不设置（create）/ 保持原值（update） */
    toolTimeout: "",
    projectId: GLOBAL_SCOPE,
  };
}

const mcpTransportOptions = [
  { value: "stdio", label: "stdio（本地子进程）" },
  { value: "http", label: "http（Streamable HTTP）" },
];

const mcpFormIntro = computed(() =>
  mcpForm.value.transport === "http"
    ? "连接远程 Streamable HTTP 端点（http transport），Agent 在对话中按需调用其工具。"
    : "以本地子进程方式启动（stdio transport），Agent 在对话中按需调用其工具。",
);

function selectMcpTransport(value: string) {
  mcpForm.value.transport = value;
}

// 进入 mcp 视图时拉取列表（连接状态是实时派生值，每次进入都刷新）。
// 离开表单视图（Cancel / goBack / 切作用域 / 编辑目标失效退回）时清表单
// 错误：提交失败的信息不残留到列表视图，也不带进下一次进表单。
watch(
  () => view.value.name,
  (name, prev) => {
    if (name === "mcp") void fetchMcpServers();
    if (prev === "mcp-edit" && name !== "mcp-edit") {
      mcpFormError.value = null;
      oauthError.value = null;
    }
  },
);

// 弹窗重开时若停在 mcp 系视图同样要拉新：view.name 未变不会触发上面的
// watch，会渲染模块级缓存的陈旧状态。两条 watch 触发条件互斥（开合只变
// visible、切视图只变 view.name），不会重复请求。mcp-edit 一并拉新是为了
// 让「编辑目标已被他处删除时退回列表」的兜底 watch 拿到最新列表。
watch(
  () => props.visible,
  (open) => {
    // 直接关弹窗（DialogClose/Esc/遮罩）也清表单错误，不留到下次打开
    if (!open) {
      mcpFormError.value = null;
      oauthError.value = null;
    }
    if (open && (view.value.name === "mcp" || view.value.name === "mcp-edit")) {
      void fetchMcpServers();
    }
  },
);

const editingServer = computed(() => {
  const current = view.value;
  if (current.name !== "mcp-edit" || !current.id) return null;
  return mcpServers.value.find((s) => s.id === current.id) ?? null;
});

// 编辑目标已被删除（如他处同步）时退回列表
watch(editingServer, (value) => {
  if (view.value.name === "mcp-edit" && view.value.id && !value) {
    view.value = { name: "mcp" };
  }
});

/** 当前作用域可见的 servers：全局 = project_id 为空；项目 = 归属所选项目。 */
const scopedMcpServers = computed(() => {
  if (!inProjectScope.value) {
    return mcpServers.value.filter((server) => !server.project_id);
  }
  const projectId = scopeId.value;
  return mcpServers.value.filter((server) => server.project_id === projectId);
});

const filteredMcpServers = computed(() => {
  const q = query.value.trim().toLowerCase();
  return scopedMcpServers.value.filter(
    (server) => !q || server.name.toLowerCase().includes(q),
  );
});

const mcpEmptyText = computed(() => {
  if (inProjectScope.value) {
    const name = activeScopeProjectName.value;
    return name
      ? `项目「${name}」下尚未配置任何 MCP server。`
      : "尚未配置任何 MCP server。";
  }
  return "全局作用域下尚未配置任何 MCP server。";
});

const projectScopeOptions = computed(() => [
  { value: GLOBAL_SCOPE, label: "全局（所有项目）" },
  ...projects.value.map((p) => ({ value: p.id, label: p.name })),
]);

function scopeLabel(server: McpServerResponse): string {
  if (!server.project_id) return "全局";
  return (
    projects.value.find((p) => p.id === server.project_id)?.name ?? "未知项目"
  );
}

function startAddServer() {
  mcpForm.value = emptyMcpForm();
  // 表单作用域默认跟随当前视图：项目作用域下新增默认归属该项目
  if (scopeId.value !== GLOBAL_SCOPE) {
    mcpForm.value.projectId = scopeId.value;
  }
  mcpFormError.value = null;
  view.value = { name: "mcp-edit", id: null };
}

function startEditServer(server: McpServerResponse) {
  mcpForm.value = {
    name: server.name,
    transport: server.transport === "http" ? "http" : "stdio",
    command: server.command ?? "",
    args: (server.args ?? []).join("\n"),
    env: Object.entries(server.env ?? {})
      .map(([key, value]) => `${key}=${value}`)
      .join("\n"),
    url: server.url ?? "",
    headers: Object.entries(server.headers ?? {})
      .map(([key, value]) => `${key}=${value}`)
      .join("\n"),
    toolTimeout:
      server.tool_timeout_ms != null ? String(server.tool_timeout_ms) : "",
    projectId: server.project_id ?? GLOBAL_SCOPE,
  };
  mcpFormError.value = null;
  view.value = { name: "mcp-edit", id: server.id };
}

/* ---------- 状态栏菜单「配置」深链：打开即定位到指定 server 的编辑表单 ---------- */
watch(
  () => [props.visible, props.focusServerId] as const,
  ([open, focusId]) => {
    if (!open || !focusId) return;
    // 先拉最新列表：startEditServer 需要完整行数据，且定位目标可能在他处
    // 刚被改过。找不到（已删除）时落到 mcp 列表视图兜底。
    void fetchMcpServers().then(() => {
      const server = mcpServers.value.find((item) => item.id === focusId);
      if (server) {
        // 列表作用域跟到该 server 的归属，从编辑表单返回列表时看得到它
        scopeId.value = server.project_id ?? GLOBAL_SCOPE;
        startEditServer(server);
      } else {
        view.value = { name: "mcp" };
      }
    });
  },
);

function selectMcpScope(value: string) {
  mcpForm.value.projectId = value;
}

function parseArgsLines(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

/** KEY=value 行 → 对象；与 parseEnvLines 同构（env 与 http headers 共用）。 */
function parseKeyValueLines(text: string): Record<string, string> {
  const map: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const eq = trimmed.indexOf("=");
    const key = eq === -1 ? trimmed : trimmed.slice(0, eq).trim();
    // 格式非法的行在 canSubmitMcp 已拦截；这里防御性跳过
    if (eq !== -1 && key) map[key] = trimmed.slice(eq + 1);
  }
  return map;
}

const canSubmitMcp = computed(() => {
  if (mcpForm.value.name.trim().length === 0) return false;
  // 必填字段随 transport：stdio 要 command，http 要 url
  if (mcpForm.value.transport === "http") {
    if (mcpForm.value.url.trim().length === 0) return false;
  } else if (mcpForm.value.command.trim().length === 0) {
    return false;
  }
  // KEY=value 行格式（env 与 headers 同规则）：空行或 KEY=value
  const linesValid = (text: string) =>
    text.split("\n").every((line) => {
      const trimmed = line.trim();
      return !trimmed || trimmed.includes("=");
    });
  return linesValid(mcpForm.value.env) && linesValid(mcpForm.value.headers);
});

async function submitMcpServer() {
  if (!canSubmitMcp.value || mcpSubmitting.value) return;
  mcpFormError.value = null;

  let toolTimeout: number | null = null;
  if (mcpForm.value.toolTimeout.trim()) {
    toolTimeout = Number.parseInt(mcpForm.value.toolTimeout, 10);
    if (!Number.isFinite(toolTimeout) || toolTimeout <= 0) {
      mcpFormError.value = "tool_timeout_ms 需为正整数（毫秒）。";
      return;
    }
  }

  const args = parseArgsLines(mcpForm.value.args);
  const env = parseKeyValueLines(mcpForm.value.env);
  const headers = parseKeyValueLines(mcpForm.value.headers);
  const projectId =
    mcpForm.value.projectId === GLOBAL_SCOPE ? null : mcpForm.value.projectId;
  const name = mcpForm.value.name.trim();
  const transport = mcpForm.value.transport;
  const command = mcpForm.value.command.trim();
  const url = mcpForm.value.url.trim();

  mcpSubmitting.value = true;
  try {
    if (view.value.name === "mcp-edit" && view.value.id) {
      // 编辑：所见即所得——只提交当前 transport 的字段组（另一组的存量
      // 由后端保留，transport 来回切不丢配置）。空 args/env/headers 传空
      // 集合清空；timeout/作用域留空则省略字段保持原值（后端 PATCH
      // 语义：提供即替换、缺省保持）。
      const ok = await updateServer(view.value.id, {
        name,
        transport,
        ...(transport === "http" ? { url, headers } : { command, args, env }),
        ...(toolTimeout != null ? { tool_timeout_ms: toolTimeout } : {}),
        ...(projectId ? { project_id: projectId } : {}),
      });
      if (ok) {
        view.value = { name: "mcp" };
      } else {
        // 变更错误由表单接管（mcpError 是模块级单例，不清会残留到列表顶部）
        mcpFormError.value = mcpError.value ?? "更新 MCP server 失败";
        clearMcpError();
      }
    } else {
      const created = await createServer({
        name,
        transport,
        ...(transport === "http"
          ? {
              url,
              ...(Object.keys(headers).length > 0 ? { headers } : {}),
            }
          : {
              command,
              ...(args.length > 0 ? { args } : {}),
              ...(Object.keys(env).length > 0 ? { env } : {}),
            }),
        ...(toolTimeout != null ? { tool_timeout_ms: toolTimeout } : {}),
        ...(projectId ? { project_id: projectId } : {}),
      });
      if (created) {
        view.value = { name: "mcp" };
      } else {
        mcpFormError.value = mcpError.value ?? "创建 MCP server 失败";
        clearMcpError();
      }
    }
  } finally {
    mcpSubmitting.value = false;
  }
}

function toggleMcpEnabled(server: McpServerResponse) {
  void toggleMcpServerEnabled(server, !server.enabled);
}

/* ---------- OAuth（http 行编辑表单）：状态徽章 / 登录 / 退出 ---------- */
//（oauthBusy/oauthWaiting/oauthError 的声明在 MCP 表单状态区，见上。）

/** 编辑目标的 OAuth 徽章状态（waitForOauth 会刷新列表行，徽章随之响应）。 */
const oauthState = computed(() => {
  const server = editingServer.value;
  return server ? mcpOauthState(server) : null;
});

const oauthExpiryFormatter = new Intl.DateTimeFormat(undefined, {
  dateStyle: "short",
  timeStyle: "short",
});

function formatOauthExpiry(date: Date): string {
  return oauthExpiryFormatter.format(date);
}

/**
 * 登录：POST start 拿 authorization_url → 新标签页打开授权服务器 →
 * 轮询本行详情直到回调把 token 入库（徽章翻为已授权）。发起失败（发现/
 * 注册 502 等）与等待超时分开呈现。
 */
async function loginOauth() {
  const server = editingServer.value;
  if (!server || oauthBusy.value) return;
  oauthError.value = null;
  oauthBusy.value = true;
  try {
    const authorizationUrl = await startMcpOauth(server.id);
    window.open(authorizationUrl, "_blank", "noopener,noreferrer");
    oauthWaiting.value = true;
    const ready = await waitForMcpOauth(server.id);
    if (!ready) {
      oauthError.value =
        "等待授权完成超时（或该 server 已被删除），可重试登录。";
    }
  } catch (error) {
    oauthError.value =
      error instanceof Error ? error.message : "发起 OAuth 登录失败";
  } finally {
    oauthBusy.value = false;
    oauthWaiting.value = false;
  }
}

async function logoutOauthLogin() {
  const server = editingServer.value;
  if (!server || oauthBusy.value) return;
  oauthError.value = null;
  const ok = await logoutMcpOauth(server.id);
  if (!ok) {
    oauthError.value = mcpError.value ?? "退出 OAuth 登录失败";
  }
}

function removeMcpServer(server: McpServerResponse) {
  void deleteServer(server.id);
}

function startConnect() {
  connectMode.value = "pick";
  pickId.value = null;
  pickKey.value = "";
  addForm.value = { type: "openai", name: "", base_url: "", api_key: "" };
  addError.value = null;
  view.value = { name: "connect-provider" };
}

async function connectPicked() {
  const provider = picked.value;
  const key = pickKey.value.trim();
  if (!provider || !key) return;
  const ok = await updateApiKey(provider.id, key);
  if (ok) {
    pickId.value = null;
    pickKey.value = "";
    view.value = { name: "provider", id: provider.id };
  }
}

function switchType(type: (typeof PROVIDER_TYPES)[number]["type"]) {
  addForm.value.type = type;
  // 切换类型时填充对应的默认 base URL
  addForm.value.base_url = TYPE_DEFAULT_URLS[type];
}

const canSubmitProvider = computed(
  () =>
    addForm.value.name.trim().length > 0 &&
    addForm.value.base_url.trim().length > 0 &&
    addForm.value.api_key.trim().length > 0,
);

// 提到模块作用域，避免每次调用重新编译正则
const SLUG_INVALID_CHARS = /[^a-z0-9]+/g;
const SLUG_EDGE_DASHES = /(^-|-$)/g;

function slugify(name: string): string {
  return (
    name
      .trim()
      .toLowerCase()
      .replaceAll(SLUG_INVALID_CHARS, "-")
      .replaceAll(SLUG_EDGE_DASHES, "") || "custom"
  );
}

async function submitProvider() {
  creating.value = true;
  addError.value = null;
  const name = addForm.value.name.trim();
  const created = await createProvider({
    id: slugify(name),
    name,
    base_url: addForm.value.base_url.trim(),
    api_key: addForm.value.api_key.trim(),
  });
  creating.value = false;
  if (!created) {
    addError.value = "创建失败：ID 可能已被占用，换个名称试试。";
    return;
  }
  view.value = { name: "provider", id: created.id };
}

async function submitModel(providerId: string) {
  const modelId = modelForm.value.model_id.trim();
  if (!modelId) return;
  const context = Number.parseInt(modelForm.value.limit_context, 10);
  const ok = await createModel(providerId, {
    model_id: modelId,
    name: modelForm.value.name.trim() || modelId,
    limit_context: Number.isFinite(context) ? context : null,
  });
  if (ok) {
    modelForm.value = { model_id: "", name: "", limit_context: "" };
  }
}

function enabledCount(provider: ProviderResponse): number {
  return provider.models.filter((model) => model.is_enabled).length;
}

function saveKey(providerId: string) {
  const key = keyInputs.value[providerId]?.trim();
  if (!key) return;
  void updateApiKey(providerId, key);
  delete keyInputs.value[providerId];
}

function handleUpdateOpen(open: boolean) {
  if (!open) emit("close");
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(0)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(0)}K`;
  return String(n);
}
</script>

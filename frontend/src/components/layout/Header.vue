<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import * as api from '@/lib/api'

const config = ref({ provider_spec: '', working_dir: '' })
const editing = ref(false)
const editValue = ref('')

async function loadConfig() {
  config.value = await api.getConfig()
}

function startEdit() {
  editValue.value = config.value.provider_spec
  editing.value = true
}

async function saveEdit() {
  try {
    config.value = await api.updateConfig(editValue.value)
    editing.value = false
  } catch (e) {
    alert(`Failed to update: ${e}`)
  }
}

function cancelEdit() {
  editing.value = false
}

onMounted(loadConfig)
</script>

<template>
  <header class="flex items-center justify-between border-b px-4 py-2 bg-background">
    <div class="flex items-center gap-3">
      <h1 class="text-lg font-bold">Oct Agent</h1>
      <Badge variant="outline" class="text-xs">{{ config.working_dir }}</Badge>
    </div>
    <div class="flex items-center gap-2">
      <template v-if="!editing">
        <Badge variant="secondary" class="text-xs cursor-pointer" @click="startEdit">
          🤖 {{ config.provider_spec }}
        </Badge>
      </template>
      <template v-else>
        <Input
          v-model="editValue"
          class="h-7 w-64 text-xs"
          placeholder="provider:model"
          @keydown.enter="saveEdit"
          @keydown.escape="cancelEdit"
        />
        <Button size="sm" variant="outline" class="h-7 text-xs" @click="saveEdit">Save</Button>
        <Button size="sm" variant="ghost" class="h-7 text-xs" @click="cancelEdit">Cancel</Button>
      </template>
    </div>
  </header>
</template>

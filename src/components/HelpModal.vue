<script setup lang="ts">
import { computed, watch, onMounted, onBeforeUnmount } from "vue";
import { marked } from "marked";
import manualRaw from "../assets/docs/usermanual.md?raw";

const props = defineProps<{ visible: boolean }>();
const emit = defineEmits<{ (e: "close"): void }>();

const html = computed(() => marked.parse(manualRaw, { async: false }) as string);

function close() {
  emit("close");
}

function onKey(e: KeyboardEvent) {
  if (e.key === "Escape" && props.visible) close();
}

onMounted(() => window.addEventListener("keydown", onKey));
onBeforeUnmount(() => window.removeEventListener("keydown", onKey));

// 打开时锁定背景滚动
watch(
  () => props.visible,
  (v) => {
    document.body.style.overflow = v ? "hidden" : "";
  }
);
</script>

<template>
  <div v-if="visible" class="modal-mask help-mask" @click.self="close">
    <div class="modal help-modal" role="dialog" aria-modal="true" aria-label="使用文档">
      <div class="help-head">
        <span class="help-title">使用文档</span>
        <button class="help-close" title="关闭" @click="close">×</button>
      </div>
      <div class="help-body" v-html="html"></div>
    </div>
  </div>
</template>

<style scoped>
.help-mask {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.55);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 200;
}
.help-modal {
  width: min(860px, 94vw);
  height: min(88vh, 960px);
  background: var(--panel);
  border: 1px solid var(--border);
  border-radius: 12px;
  box-shadow: 0 16px 48px rgba(0, 0, 0, 0.5);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.help-head {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  background: #1c2128;
  border-bottom: 1px solid var(--border);
}
.help-title {
  font-size: 15px;
  font-weight: 600;
  color: var(--fg);
}
.help-close {
  width: 28px;
  height: 28px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--fg-dim);
  font-size: 20px;
  line-height: 1;
  cursor: pointer;
}
.help-close:hover {
  background: rgba(255, 255, 255, 0.08);
  color: var(--fg);
}
.help-body {
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
  padding: 20px 26px 32px;
  line-height: 1.7;
  font-size: 14px;
  color: var(--fg);
}
/* Markdown 排版（沿用应用配色令牌，主题一致） */
.help-body :deep(h1) {
  font-size: 22px;
  margin: 0 0 14px;
  padding-bottom: 10px;
  border-bottom: 1px solid var(--border);
}
.help-body :deep(h2) {
  font-size: 17px;
  margin: 22px 0 10px;
  color: #cdd9e5;
}
.help-body :deep(h3) {
  font-size: 15px;
  margin: 16px 0 8px;
  color: #cdd9e5;
}
.help-body :deep(p) {
  margin: 8px 0;
}
.help-body :deep(ul),
.help-body :deep(ol) {
  margin: 8px 0;
  padding-left: 22px;
}
.help-body :deep(li) {
  margin: 4px 0;
}
.help-body :deep(a) {
  color: var(--accent);
}
.help-body :deep(blockquote) {
  margin: 10px 0;
  padding: 8px 14px;
  border-left: 3px solid var(--accent);
  background: rgba(47, 129, 247, 0.08);
  color: var(--fg-dim);
  border-radius: 0 6px 6px 0;
}
.help-body :deep(code) {
  font-family: "SFMono-Regular", Consolas, "Liberation Mono", monospace;
  font-size: 12.5px;
  background: #0d1117;
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 1px 5px;
}
.help-body :deep(pre) {
  background: #0d1117;
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px 14px;
  overflow: auto;
}
.help-body :deep(pre code) {
  border: none;
  padding: 0;
  background: transparent;
}
.help-body :deep(table) {
  width: 100%;
  border-collapse: collapse;
  margin: 12px 0;
  font-size: 13px;
}
.help-body :deep(th),
.help-body :deep(td) {
  border: 1px solid var(--border);
  padding: 7px 10px;
  text-align: left;
  vertical-align: top;
}
.help-body :deep(th) {
  background: #1c2128;
  color: #cdd9e5;
  font-weight: 600;
}
.help-body :deep(hr) {
  border: none;
  border-top: 1px solid var(--border);
  margin: 18px 0;
}
.help-body :deep(em) {
  color: var(--fg-dim);
}
</style>

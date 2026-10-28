<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import Icon from './Icon.vue'

const props = defineProps<{
  channel: 'a' | 'b'
  seq: number
  fps: number
  width: number
  height: number
  available: boolean
}>()

type FeedState = 'connecting' | 'live' | 'offline'

const pane = ref<HTMLElement | null>(null)
const imgEl = ref<HTMLImageElement | null>(null)
const state = ref<FeedState>('connecting')
const src = ref('')
const fill = ref(false)
const tries = ref(0)
let retry: number | undefined
let watchdog: number | undefined
let probe: number | undefined

const title = computed(() => props.channel.toUpperCase())
const slot = computed(() => (props.channel === 'a' ? '槽 0' : '槽 1'))
const resolution = computed(() =>
  props.width && props.height ? `${props.width} × ${props.height}` : '— × —',
)
const stateText = computed(() => {
  if (state.value === 'live') return '实时'
  if (state.value === 'offline') return '重连中'
  return '连接中'
})
const pad = (n: number) => String(n).padStart(6, '0')

function connect() {
  window.clearTimeout(retry)
  if (!props.available) return
  state.value = state.value === 'live' ? 'live' : 'connecting'
  tries.value += 1
  // 加时间戳绕开任何中间缓存；服务端会忽略查询串
  src.value = `/stream_${props.channel}.mjpg?t=${Date.now()}`
  window.clearTimeout(watchdog)
  watchdog = window.setTimeout(() => {
    if (state.value !== 'live') state.value = 'offline'
  }, 5000)
  // Firefox/Safari 的 multipart MJPEG 不一定触发 img.load，
  // 用 naturalWidth 作为“第一帧已经解码”的兜底信号。
  window.clearInterval(probe)
  probe = window.setInterval(() => {
    if (imgEl.value?.naturalWidth) onLoad()
  }, 250)
}

function onLoad() {
  state.value = 'live'
  window.clearTimeout(watchdog)
  window.clearInterval(probe)
}

function onError() {
  state.value = 'offline'
  window.clearTimeout(retry)
  window.clearInterval(probe)
  retry = window.setTimeout(connect, 1500)
}

function snapshot() {
  const a = document.createElement('a')
  const stamp = new Date().toISOString().replace(/[:.]/g, '-')
  a.href = `/snapshot_${props.channel}.jpg`
  a.download = `cam-${props.channel}-${stamp}.jpg`
  document.body.appendChild(a)
  a.click()
  a.remove()
}

async function toggleFullscreen() {
  const el = pane.value
  if (!el) return
  try {
    if (document.fullscreenElement === el) await document.exitFullscreen()
    else await el.requestFullscreen()
  } catch {
    /* 浏览器拒绝时保持原布局 */
  }
}

defineExpose({ snapshot, toggleFullscreen })

watch(() => props.available, (ok) => { if (ok) connect() })
onMounted(connect)
onBeforeUnmount(() => {
  window.clearTimeout(retry)
  window.clearTimeout(watchdog)
  window.clearInterval(probe)
})
</script>

<template>
  <article ref="pane" class="pane" :class="[`accent-${channel}`, `is-${state}`]">
    <header class="pane-head">
      <span class="cam-id">{{ title }}</span>
      <div class="cam-title">
        <strong>CAM {{ title }}</strong>
        <span>{{ slot }} · RAW10</span>
      </div>
      <span class="pane-state" :class="state"><i />{{ stateText }}</span>
      <div class="pane-rate">
        <b>{{ fps.toFixed(1) }}</b>
        <span>fps</span>
      </div>
    </header>

    <div class="viewport" @dblclick="toggleFullscreen">
      <img
        v-if="available && src"
        ref="imgEl"
        class="feed"
        :class="{ fill }"
        :src="src"
        :alt="`CAM ${title} 实时画面`"
        decoding="async"
        draggable="false"
        @load="onLoad"
        @error="onError"
      />
      <div class="scan" />
      <div class="vignette" />
      <div class="corners"><i /><i /><i /><i /></div>

      <div class="hud hud-top">
        <span class="chip">{{ resolution }}</span>
        <span class="chip chip-seq">SEQ {{ pad(seq) }}</span>
      </div>

      <div v-if="available" class="hud hud-bottom">
        <button class="hud-btn" type="button" @click.stop="snapshot">
          <Icon name="camera" :size="15" />抓拍
        </button>
        <button class="hud-btn" type="button" :class="{ on: fill }" @click.stop="fill = !fill">
          <Icon name="fill" :size="15" />{{ fill ? '适应' : '填充' }}
        </button>
        <button class="hud-btn" type="button" @click.stop="toggleFullscreen">
          <Icon name="expand" :size="15" />全屏
        </button>
      </div>

      <div v-if="!available || state !== 'live'" class="screen-msg">
        <template v-if="!available">
          <strong>B 路未启用</strong>
          <span>用 --source device-pair 启动后显示</span>
        </template>
        <template v-else>
          <span class="ring" :class="state" />
          <strong>{{ state === 'offline' ? '视频流已断开' : '正在接入视频流' }}</strong>
          <span>{{ state === 'offline' ? `第 ${tries} 次重试 · 1.5 s 后重连` : '等待第一帧…' }}</span>
        </template>
      </div>
    </div>
  </article>
</template>

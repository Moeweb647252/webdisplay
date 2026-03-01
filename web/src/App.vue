<script setup>
import { onMounted, onUnmounted, ref } from 'vue'

import { UltraLowLatencyPlayer, createUiState } from './player'

const state = createUiState()
const streamCanvas = ref(null)

let player = null

onMounted(() => {
  if (!streamCanvas.value) {
    console.error('未找到流媒体画布')
    return
  }

  player = new UltraLowLatencyPlayer(streamCanvas.value, state)
})

onUnmounted(() => {
  if (player) {
    player.destroy()
    player = null
  }
})

const selectMonitor = (index) => {
  if (player) {
    player.selectMonitor(index)
  }
}

const applyEncoding = () => {
  if (player) {
    player.applyEncodingSettings()
  }
}

const resetEncoding = () => {
  if (player) {
    player.resetEncodingSettings()
  }
}

const codecLabel = (codecId) => {
  const matched = state.availableCodecs.find((item) => item.id === codecId)
  return matched ? matched.label : '--'
}
</script>

<template>
  <main class="stream-page">
    <canvas id="stream-canvas" ref="streamCanvas" :class="{ 'show-local-cursor': state.showLocalCursor }" />

    <div id="overlay" :class="{ hidden: !state.overlayVisible }">
      <div class="stat-row">
        <span class="stat-label">延迟</span>
        <span class="stat-value">{{ state.stats.latency }}</span>
      </div>
      <div class="stat-row">
        <span class="stat-label">FPS</span>
        <span class="stat-value" :class="state.stats.fpsClass">{{ state.stats.fps }}</span>
      </div>
      <div class="stat-row">
        <span class="stat-label">解码</span>
        <span class="stat-value">{{ state.stats.decode }}</span>
      </div>
      <div class="stat-row">
        <span class="stat-label">帧队列</span>
        <span class="stat-value">{{ state.stats.queue }}</span>
      </div>
      <div class="stat-row">
        <span class="stat-label">码率</span>
        <span class="stat-value">{{ state.stats.bitrate }}</span>
      </div>
    </div>

    <div id="connection-status" v-show="state.connectionVisible">
      <span class="dot" :class="state.connected ? 'connected' : 'disconnected'" />
      {{ state.connectionText }}
      <div v-if="state.connectionDetail" class="connection-detail">{{ state.connectionDetail }}</div>
    </div>

    <div id="monitor-picker" :class="{ hidden: !state.monitorPickerVisible }">
      <h3>选择显示器</h3>
      <div id="monitor-list">
        <div v-if="state.monitors.length === 0" class="monitor-empty">未检测到显示器</div>

        <div v-for="monitor in state.monitors" :key="monitor.index" class="monitor-card"
          :class="{ active: monitor.index === state.activeMonitorIndex }" @click="selectMonitor(monitor.index)">
          <div class="monitor-info">
            <div class="monitor-name">
              显示器 {{ monitor.index }}: {{ monitor.name }} {{ monitor.primary ? '(主屏)' : '' }}
            </div>
            <div class="monitor-res">{{ monitor.width }} x {{ monitor.height }}</div>
          </div>
          <button class="monitor-switch-btn" type="button">切换</button>
        </div>
      </div>

      <div class="monitor-hint">Ctrl+Alt+Shift+M 切换显示面板</div>
    </div>

    <div id="encoding-panel" :class="{ hidden: !state.encodingPanelVisible }">
      <h3>编码设置</h3>

      <div class="encoding-field">
        <div class="encoding-label">
          <label for="encoding-codec">编码格式</label>
          <span class="encoding-label-value">{{ codecLabel(state.encodingDraft.codec) }}</span>
        </div>
        <select id="encoding-codec" v-model="state.encodingDraft.codec">
          <option v-if="state.availableCodecs.length === 0" disabled value="">检测中...</option>
          <option v-for="codec in state.availableCodecs" :key="codec.id" :value="codec.id">
            {{ codec.label }}
          </option>
        </select>
      </div>

      <div class="encoding-field">
        <div class="encoding-label">
          <label for="encoding-bitrate">目标码率</label>
          <span class="encoding-label-value">{{ state.encodingDraft.bitrateMbps }} Mbps</span>
        </div>
        <input id="encoding-bitrate" v-model.number="state.encodingDraft.bitrateMbps" type="range" min="2" max="80"
          step="1">
      </div>

      <div class="encoding-field">
        <div class="encoding-label">
          <label for="encoding-fps">目标帧率</label>
          <span class="encoding-label-value">{{ state.encodingDraft.fps }} FPS</span>
        </div>
        <input id="encoding-fps" v-model.number="state.encodingDraft.fps" type="range" min="24" max="120" step="6">
      </div>

      <div class="encoding-field">
        <div class="encoding-label">
          <label for="encoding-keyint">关键帧间隔</label>
          <span class="encoding-label-value">{{ state.encodingDraft.keyframeInterval }} 秒</span>
        </div>
        <input id="encoding-keyint" v-model.number="state.encodingDraft.keyframeInterval" type="number" min="1" max="10"
          step="1">
      </div>

      <div class="encoding-actions">
        <button id="encoding-apply" type="button" @click="applyEncoding">应用</button>
        <button id="encoding-reset" type="button" @click="resetEncoding">默认</button>
      </div>

      <div class="monitor-hint">Ctrl+Alt+Shift+E 切换编码面板</div>
    </div>

    <div id="control-hint" :class="{ hidden: !state.controlHintVisible }">
      {{ state.controlHintText }}
    </div>
  </main>
</template>

<style>
html,
body,
#app {
  width: 100%;
  height: 100%;
  margin: 0;
}

* {
  box-sizing: border-box;
}

body {
  background: #0a0a0a;
  color: #e0e0e0;
  font-family: 'Segoe UI', system-ui, sans-serif;
  overflow: hidden;
}

.stream-page {
  width: 100%;
  height: 100%;
}

#stream-canvas {
  width: 100%;
  height: 100%;
  display: block;
  cursor: none;
  object-fit: contain;
}

#stream-canvas.show-local-cursor {
  cursor: default;
}

#overlay {
  position: fixed;
  top: 12px;
  left: 12px;
  background: rgba(0, 0, 0, 0.75);
  padding: 10px 16px;
  border-radius: 8px;
  font-size: 13px;
  font-family: 'Consolas', monospace;
  z-index: 100;
  backdrop-filter: blur(8px);
  transition: opacity 0.3s;
}

#overlay.hidden {
  opacity: 0;
  pointer-events: none;
}

.stat-row {
  display: flex;
  justify-content: space-between;
  gap: 24px;
}

.stat-label {
  color: #888;
}

.stat-value {
  color: #4fc3f7;
  font-weight: bold;
}

.stat-value.warn {
  color: #ffb74d;
}

.stat-value.bad {
  color: #ef5350;
}

#connection-status {
  position: fixed;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  font-size: 18px;
  text-align: center;
}

.connection-detail {
  margin-top: 8px;
  color: #b0bec5;
  font-size: 14px;
}

.dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  margin-right: 6px;
}

.dot.connected {
  background: #4caf50;
}

.dot.disconnected {
  background: #f44336;
}

#monitor-picker {
  position: fixed;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  background: rgba(20, 20, 20, 0.95);
  padding: 24px;
  border-radius: 12px;
  z-index: 200;
  backdrop-filter: blur(12px);
  border: 1px solid rgba(255, 255, 255, 0.1);
  min-width: 300px;
  box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
  transition: opacity 0.3s, transform 0.3s;
}

#monitor-picker.hidden {
  opacity: 0;
  pointer-events: none;
  transform: translate(-50%, -45%);
}

#monitor-picker h3 {
  margin-bottom: 16px;
  font-size: 18px;
  font-weight: 500;
  color: #fff;
  text-align: center;
}

.monitor-card {
  background: rgba(255, 255, 255, 0.05);
  padding: 12px 16px;
  border-radius: 8px;
  margin-bottom: 10px;
  cursor: pointer;
  border: 1px solid transparent;
  transition: all 0.2s;
  display: flex;
  justify-content: space-between;
  align-items: center;
}

.monitor-card:hover {
  background: rgba(255, 255, 255, 0.1);
  border-color: rgba(255, 255, 255, 0.2);
}

.monitor-card.active {
  border-color: #4fc3f7;
  background: rgba(79, 195, 247, 0.1);
}

.monitor-info {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.monitor-name {
  font-weight: 500;
  font-size: 14px;
}

.monitor-res {
  font-size: 12px;
  color: #888;
}

.monitor-hint {
  text-align: center;
  font-size: 12px;
  color: #666;
  margin-top: 16px;
}

.monitor-empty {
  color: #888;
  text-align: center;
  padding: 8px 0;
}

.monitor-switch-btn {
  background: #4fc3f7;
  color: #000;
  border: none;
  border-radius: 4px;
  padding: 6px 12px;
  cursor: pointer;
  font-weight: 500;
  font-size: 12px;
}

#encoding-panel {
  position: fixed;
  top: 12px;
  right: 12px;
  width: min(340px, calc(100vw - 24px));
  background: rgba(20, 20, 20, 0.95);
  padding: 18px;
  border-radius: 12px;
  z-index: 210;
  backdrop-filter: blur(12px);
  border: 1px solid rgba(255, 255, 255, 0.1);
  box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
  transition: opacity 0.25s, transform 0.25s;
}

#encoding-panel.hidden {
  opacity: 0;
  pointer-events: none;
  transform: translateY(-8px);
}

#encoding-panel h3 {
  margin-bottom: 12px;
  font-size: 17px;
  font-weight: 500;
  color: #fff;
}

.encoding-field {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin-bottom: 12px;
}

.encoding-label {
  display: flex;
  justify-content: space-between;
  align-items: baseline;
  font-size: 13px;
  color: #cfd8dc;
}

.encoding-label-value {
  color: #4fc3f7;
  font-family: 'Consolas', monospace;
  font-size: 12px;
}

.encoding-field input[type='range'] {
  width: 100%;
  accent-color: #4fc3f7;
}

.encoding-field input[type='number'] {
  width: 100%;
  padding: 6px 8px;
  border-radius: 6px;
  border: 1px solid rgba(255, 255, 255, 0.18);
  background: rgba(255, 255, 255, 0.08);
  color: #fff;
  font-size: 13px;
}

.encoding-field select {
  width: 100%;
  padding: 6px 8px;
  border-radius: 6px;
  border: 1px solid rgba(255, 255, 255, 0.18);
  background: rgba(255, 255, 255, 0.08);
  color: #fff;
  font-size: 13px;
}

.encoding-actions {
  display: flex;
  gap: 8px;
  margin-top: 14px;
}

.encoding-actions button {
  flex: 1;
  border: none;
  border-radius: 6px;
  padding: 8px 10px;
  cursor: pointer;
  font-size: 13px;
  font-weight: 500;
  transition: transform 0.15s ease, opacity 0.15s ease;
}

#encoding-apply {
  background: #4fc3f7;
  color: #001018;
}

#encoding-reset {
  background: rgba(255, 255, 255, 0.08);
  color: #d8d8d8;
  border: 1px solid rgba(255, 255, 255, 0.2);
}

.encoding-actions button:hover {
  opacity: 0.92;
  transform: translateY(-1px);
}

#control-hint {
  position: fixed;
  left: 50%;
  bottom: 14px;
  transform: translateX(-50%);
  background: rgba(0, 0, 0, 0.6);
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 6px;
  padding: 6px 12px;
  font-size: 12px;
  color: #9e9e9e;
  font-family: 'Consolas', monospace;
  z-index: 90;
  backdrop-filter: blur(6px);
  opacity: 1;
  transition: opacity 0.25s ease, transform 0.25s ease;
}

#control-hint.hidden {
  opacity: 0;
  transform: translateX(-50%) translateY(8px);
  pointer-events: none;
}

@media (max-width: 900px) {
  #overlay {
    top: 8px;
    left: 8px;
    padding: 8px 10px;
    font-size: 12px;
  }

  #monitor-picker {
    width: calc(100vw - 20px);
    min-width: 0;
    padding: 18px;
  }

  #encoding-panel {
    top: auto;
    right: 8px;
    bottom: 46px;
    left: 8px;
    width: auto;
  }

  #control-hint {
    left: 8px;
    right: 8px;
    transform: none;
    text-align: center;
  }

  #control-hint.hidden {
    transform: translateY(8px);
  }
}
</style>

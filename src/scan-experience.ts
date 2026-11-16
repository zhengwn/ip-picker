import { defaultConfig, type ScanConfig, type ScanRun } from './types'
import { completeMeasurement, stageProgressText } from './ui-state'

export function scanModeLabel(run: Pick<ScanRun, 'quickValidation'>): string {
  return run.quickValidation == null ? '历史测速' : run.quickValidation ? '快速验证' : '完整优选'
}

const configFields = Object.keys(defaultConfig()) as (keyof ScanConfig)[]

/** Rust serialization and frontend defaults need not enumerate fields in the same order. */
export function sameScanConfig(left: ScanConfig, right: ScanConfig): boolean {
  return configFields.every(field => JSON.stringify(left[field]) === JSON.stringify(right[field]))
}

export function compactScanProgress(run: ScanRun) {
  const stages = run.config.stabilityFirst
    ? ['connecting', 'discovering', 'stabilizing', 'verifying', 'downloading']
    : ['connecting', 'discovering', 'verifying', 'stabilizing', 'downloading']
  const phase = stageProgressText(run)
  const { completed, total, stage } = run.progress
  const step = stages.indexOf(stage) + 1
  return {
    step: step ? `阶段 ${step} / ${stages.length}` : '',
    title: phase.title,
    caption: stage === 'stabilizing' && total > 0 ? `已探测 ${completed} / ${total} 次` : phase.caption,
    percent: total > 0 ? Math.min(100, Math.round(completed / total * 100)) : null,
    complete: run.results.filter(result => completeMeasurement(result, run.config)).length,
  }
}

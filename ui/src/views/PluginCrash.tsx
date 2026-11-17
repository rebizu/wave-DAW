// "WAVE did not exit cleanly": the plugins that were loaded when the last session died (one of
// them may be why). Offered once at start: keep a plugin out of future scans, or let it be —
// the crash may have had nothing to do with it.
import { createSignal, For, onMount, Show } from 'solid-js';
import { be } from '../api/backend';
import type { PluginCrashSuspect } from '../api/types';
import { t } from '../lib/i18n';
import { say } from '../state/ui';

export function PluginCrash() {
  const [suspects, setSuspects] = createSignal<PluginCrashSuspect[]>([]);
  const [blocked, setBlocked] = createSignal<string[]>([]);
  const [err, setErr] = createSignal('');
  // the block list is per module (format + path), not per class: two installs of one plugin
  // may share a cid but live in different bundles, and only the one the user blocked is gone
  const moduleKey = (s: PluginCrashSuspect) => `${s.format}\u0000${s.path}`;
  onMount(async () => {
    try {
      const s = await be().pluginCrashSuspects();
      if (s.length) setSuspects(s);
    } catch (e) {
      console.warn('crash suspects:', e);
    }
  });
  const block = async (s: PluginCrashSuspect) => {
    try {
      await be().pluginBlock(s.format, s.path);
      setBlocked(b => [...b, moduleKey(s)]);
      say(t('d.pcrash.blocked', { name: s.name }));
    } catch (e) {
      setErr(String(e));
    }
  };
  return (
    <Show when={suspects().length}>
      <div class="overlay" onPointerDown={() => setSuspects([])}>
        <div class="dialog" role="dialog" aria-modal="true" aria-label={t('d.pcrash.title')} onPointerDown={e => e.stopPropagation()} onKeyDown={e => { if (e.key !== 'Escape') e.stopPropagation(); }}>
          <div class="hd">
            <span>{t('d.pcrash.title')}</span>
            <button onClick={() => setSuspects([])} aria-label={t('d.pcrash.dismiss')}>×</button>
          </div>
          <p class="note">{t('d.pcrash.intro')}</p>
          <ul class="misslist">
            <For each={suspects()}>
              {s => (
                <li>
                  <div class="row misshd">
                    <span class="missname" title={s.name}>{s.name}</span>
                    <span class="tag9">{s.format.toUpperCase()}</span>
                    <div style={{ flex: 1 }} />
                    <Show when={!blocked().includes(moduleKey(s))} fallback={<span class="note">{t('d.pcrash.isBlocked')}</span>}>
                      <button class="smallbtn" onClick={() => void block(s)} title={s.path}>{t('d.pcrash.block')}</button>
                    </Show>
                  </div>
                  <div class="misspath mono" title={s.path}><bdi>{s.path}</bdi></div>
                </li>
              )}
            </For>
          </ul>
          <Show when={err()}><p class="note warn" role="alert">{err()}</p></Show>
          <div class="row" style={{ 'justify-content': 'flex-end', 'margin-top': '12px' }}>
            <button class="smallbtn primary" onClick={() => setSuspects([])}>{t('d.pcrash.dismiss')}</button>
          </div>
        </div>
      </div>
    </Show>
  );
}

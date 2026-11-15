/**
 * Application settings.
 *
 * Stands in for the original's system options (docs/02 §1). Everything tied to
 * MetaMoJi's accounts, licensing or add-on store is absent by design — those
 * belong to their commerce, not to this app.
 */

import { useEffect, useState } from "react";
import { useNavigate } from "react-router";

import { Icon } from "../components/Icon";
import * as api from "../ipc/api";
import { isTauri } from "../ipc/api";
import type { AppStatus } from "../ipc/api";
import { LOCALE_NAMES, setLocale, type Locale } from "../i18n";
import { useTranslation } from "../i18n/useTranslation";
import { usePrefs } from "../store/prefsStore";
import { useUpdateStore } from "../store/updateStore";

const AUTOSAVE_CHOICES = [1, 2, 5, 10];

export function SettingsScreen() {
  const navigate = useNavigate();
  const { t, locale } = useTranslation();
  const prefs = usePrefs();
  const [status, setStatus] = useState<AppStatus | null>(null);
  const [appVersion, setAppVersion] = useState<string | null>(null);

  useEffect(() => {
    void api.appStatus().then(setStatus).catch(() => setStatus(null));
  }, []);

  useEffect(() => {
    if (!isTauri()) return;
    void import("@tauri-apps/api/app")
      .then((app) => app.getVersion())
      .then(setAppVersion)
      .catch(() => setAppVersion(null));
  }, []);

  const updateStatus = useUpdateStore((s) => s.status);
  const updateVersion = useUpdateStore((s) => s.version);
  const updateProgress = useUpdateStore((s) => s.progress);
  const updateError = useUpdateStore((s) => s.error);
  const checkForUpdate = useUpdateStore((s) => s.check);
  const installUpdate = useUpdateStore((s) => s.install);

  return (
    <div className="app">
      <header className="topbar">
        <button
          type="button"
          className="icon-btn"
          onClick={() => navigate("/")}
          title={t("ノート一覧に戻る")}
        >
          <Icon name="arrow_back" />
          <span className="sr-only">{t("ノート一覧に戻る")}</span>
        </button>
        <span className="topbar__title">{t("設定")}</span>
      </header>

      <main className="settings">
        <section>
          <h2>{t("表示")}</h2>
          <div className="setting-row">
            <label htmlFor="locale">{t("言語")}</label>
            <select
              id="locale"
              value={locale}
              onChange={(e) => setLocale(e.target.value as Locale)}
            >
              {(Object.keys(LOCALE_NAMES) as Locale[]).map((code) => (
                <option key={code} value={code}>
                  {LOCALE_NAMES[code]}
                </option>
              ))}
            </select>
          </div>
        </section>

        <section>
          <h2>{t("編集")}</h2>
          <div className="setting-row">
            <label htmlFor="autosave">{t("自動保存の間隔")}</label>
            <select
              id="autosave"
              value={prefs.autosaveSeconds}
              onChange={(e) => prefs.setAutosaveSeconds(Number(e.target.value))}
            >
              {AUTOSAVE_CHOICES.map((seconds) => (
                <option key={seconds} value={seconds}>
                  {t("{seconds} 秒", { seconds })}
                </option>
              ))}
            </select>
          </div>

          <div className="setting-row">
            <label htmlFor="finger">{t("指で描く")}</label>
            <input
              id="finger"
              type="checkbox"
              checked={prefs.fingerDrawing}
              onChange={(e) => prefs.setFingerDrawing(e.target.checked)}
            />
          </div>
          <p className="setting-note">
            {t("ペンを検出したあとは指の接触を手のひらとして無視します。")}
          </p>
        </section>

        <section>
          <h2>{t("手書き文字認識")}</h2>
          <p className="setting-note">
            {t(
              "この版では手書きの認識は行いません。ストロークの入力・保存・表示のみに対応しています。",
            )}
          </p>
        </section>

        <section>
          <h2>{t("情報")}</h2>
          <div className="setting-row">
            <label>{t("データの保存場所")}</label>
            <code className="setting-value">{status?.dataDir ?? "—"}</code>
          </div>
          <div className="setting-row">
            <label>{t("スキーマバージョン")}</label>
            <code className="setting-value">{status?.schemaVersion ?? "—"}</code>
          </div>
          <div className="setting-row">
            <label>{t("バージョン")}</label>
            <code className="setting-value">{appVersion ?? "—"}</code>
          </div>
          <div className="setting-row">
            <label>{t("アップデート")}</label>
            {updateStatus === "available" ? (
              <button type="button" className="btn btn--text" onClick={() => void installUpdate()}>
                {t("{version} をインストール", { version: updateVersion ?? "" })}
              </button>
            ) : updateStatus === "installing" ? (
              <span>
                {updateProgress != null
                  ? t("インストール中… {progress}%", { progress: updateProgress })
                  : t("インストール中…")}
              </span>
            ) : (
              <button
                type="button"
                className="btn btn--text"
                disabled={!isTauri() || updateStatus === "checking"}
                onClick={() => void checkForUpdate()}
              >
                {updateStatus === "checking" ? t("確認中…") : t("アップデートを確認")}
              </button>
            )}
          </div>
          {updateStatus === "upToDate" && (
            <p className="setting-note">{t("最新バージョンです。")}</p>
          )}
          {updateStatus === "error" && (
            <p className="setting-note">{t("アップデートの確認に失敗しました: {error}", { error: updateError ?? "" })}</p>
          )}
        </section>
      </main>
    </div>
  );
}

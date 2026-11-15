/**
 * Floating notice for an available app update.
 *
 * Mounted once at the app root; `App` triggers the silent startup check, this
 * only renders what that check found. Dismissing it just hides it for the
 * rest of the session — the update is still installed the next time the app
 * is launched, or from Settings.
 */

import { useState } from "react";

import { Icon } from "./Icon";
import { useTranslation } from "../i18n/useTranslation";
import { useUpdateStore } from "../store/updateStore";

export function UpdateToast() {
  const { t } = useTranslation();
  const status = useUpdateStore((s) => s.status);
  const version = useUpdateStore((s) => s.version);
  const progress = useUpdateStore((s) => s.progress);
  const install = useUpdateStore((s) => s.install);
  const [dismissed, setDismissed] = useState(false);

  if (dismissed || (status !== "available" && status !== "installing")) return null;

  return (
    <div className="sync-toast" role="status">
      <Icon name="cloud_sync" />
      <div className="sync-toast__body">
        {status === "installing" ? (
          <span>
            {progress != null
              ? t("アップデートをダウンロード中… {progress}%", { progress })
              : t("アップデートをダウンロード中…")}
          </span>
        ) : (
          <span>{t("新しいバージョン {version} が利用可能です。", { version: version ?? "" })}</span>
        )}
      </div>
      {status === "available" && (
        <>
          <button type="button" className="icon-btn" onClick={() => void install()} title={t("今すぐインストール")}>
            <Icon name="download" label={t("今すぐインストール")} />
          </button>
          <button type="button" className="icon-btn" onClick={() => setDismissed(true)} title={t("閉じる")}>
            <Icon name="close" label={t("閉じる")} />
          </button>
        </>
      )}
    </div>
  );
}

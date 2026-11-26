/**
 * Sign-in state.
 *
 * docs/14 §1 catalogues five login drivers in the original — normal, simple,
 * Google, SSO and external — all built on one `LoginDriver` template method
 * that ends by writing user info into a shared holder. The same shape is kept
 * here as `LoginStrategy`: whatever the method, it produces a `CloudSession`,
 * and everything downstream is identical.
 *
 * The two that are implemented are the two that need no third-party SDK:
 *
 *   normal  学校ID + ユーザーID + パスワード   (`NormalLoginDriver`)
 *   simple  学校ID + クラス + 出席番号 + パスワード (`SimpleLoginDriver`)
 *
 * QR is a bootstrap for `normal` rather than a driver of its own — the code
 * carries a school and a user, never a password (see `parseQrPayload`).
 *
 * **The session is not persisted.** The original keeps a `qwd` — a
 * password-equivalent token that lets it sign in silently — and putting one in
 * `localStorage` would mean any code running in the webview could read it. The
 * cookie lives in the Rust process and dies with it, so the app asks again on
 * the next launch. That is the deliberate trade.
 */

import { create } from "zustand";

import { adoptSession } from "../api/client";
import * as api from "../ipc/api";
import type { CloudSession } from "../ipc/api";

export type { CloudSession };

/** Fields a strategy may read. Not every strategy uses every one. */
export interface LoginInput {
  coLoginId?: string;
  loginName?: string;
  password?: string;
  classGroupId?: string;
  idNumber?: string;
  payload?: string;
}

/**
 * One way of signing in. docs/14 §1's `LoginDriver` in trait form: the flow
 * differs, the outcome does not.
 */
export interface LoginStrategy {
  id: "normal" | "simple" | "qr" | "sso";
  label: string;
  login(input: LoginInput): Promise<CloudSession>;
}

export const normalLogin: LoginStrategy = {
  id: "normal",
  label: "学校ID とユーザーID",
  async login({ coLoginId, loginName, password }) {
    return api.cloudLogin(coLoginId ?? "", loginName ?? "", password ?? "");
  },
};

/**
 * 簡易ログイン — the account form schools hand to children: pick your class,
 * pick your number, type the class password.
 */
export const simpleLogin: LoginStrategy = {
  id: "simple",
  label: "簡易ログイン(クラスと出席番号)",
  async login({ coLoginId, classGroupId, idNumber, password }) {
    return api.cloudClassroomLogin(
      coLoginId ?? "",
      classGroupId ?? "",
      idNumber ?? "",
      password ?? "",
    );
  },
};

/**
 * QR bootstrap.
 *
 * `QRCodeParser` reads `$key=value,...` with keys `cid` (a URL carrying the
 * root server and the 学校ID), `uid` (the user id), `qwd` and `rootServer`.
 *
 * docs/14 §1 records a real defect worth repeating here: the `pwd` key is
 * parsed and the result thrown away, so a password can never arrive by QR. So
 * this fills in the school and the user and still asks for a password — which
 * is all the original could ever do either.
 */
export function parseQrPayload(text: string): Record<string, string> {
  const body = text.trim().startsWith("$") ? text.trim().slice(1) : text.trim();
  const out: Record<string, string> = {};
  for (const pair of body.split(",")) {
    const [key, ...rest] = pair.split("=");
    if (key && rest.length > 0) out[key.trim()] = rest.join("=").trim();
  }
  return out;
}

/**
 * Splits `cid` into a root server and a 学校ID.
 *
 * `LoginUtils.getRootServerUrlAndCoLoginIdFromUrl` accepts either a bare id or
 * a URL whose last path segment is the id — a school hands out one QR code and
 * it has to work whichever form the administrator pasted in.
 */
export function parseCid(cid: string): { rootServer?: string; coLoginId: string } {
  const trimmed = cid.trim();
  if (!/^https?:\/\//i.test(trimmed)) return { coLoginId: trimmed };

  try {
    const url = new URL(trimmed);
    const segments = url.pathname.split("/").filter(Boolean);
    const coLoginId = segments.pop() ?? "";
    return {
      rootServer: `${url.origin}/${segments.join("/")}${segments.length ? "/" : ""}`,
      coLoginId,
    };
  } catch {
    return { coLoginId: trimmed };
  }
}

export const qrLogin: LoginStrategy = {
  id: "qr",
  label: "QR コード",
  async login({ payload, password }) {
    const fields = parseQrPayload(payload ?? "");
    const { rootServer, coLoginId } = parseCid(fields.cid ?? "");
    const server = fields.rootServer ?? rootServer;
    if (server) await useAuthStore.getState().setRootServer(server);

    if (!coLoginId) throw new Error("QR コードに学校 ID が含まれていません");
    const loginName = fields.uid;
    if (!loginName) throw new Error("QR コードにユーザー ID が含まれていません");

    return api.cloudLogin(coLoginId, loginName, password ?? "");
  },
};

export const ssoLogin: LoginStrategy = {
  id: "sso",
  label: "SSO",
  async login() {
    // Wiring this up means registering `mmjsso://` with Tauri's deep-link
    // plugin and completing the flow on the redirect (docs/14 §4). Saying so is
    // better than a button that silently does nothing.
    throw new Error("SSO ログインはこのビルドでは未対応です");
  },
};

export const LOGIN_STRATEGIES: LoginStrategy[] = [
  normalLogin,
  simpleLogin,
  qrLogin,
  ssoLogin,
];

interface AuthState {
  rootServer: string;
  session: CloudSession | null;
  busy: boolean;
  error: string | null;

  init: () => Promise<void>;
  setRootServer: (url: string) => Promise<void>;
  signIn: (strategy: LoginStrategy, input: LoginInput) => Promise<boolean>;
  signOut: () => Promise<void>;
  clearError: () => void;
}

export const useAuthStore = create<AuthState>((set) => ({
  rootServer: "https://mps.metamoji.com/",
  session: null,
  busy: false,
  error: null,

  /**
   * Reads back what Rust holds. The session lives there, so a reload of the
   * webview must not look like a sign-out.
   */
  init: async () => {
    try {
      const [rootServer, session] = await Promise.all([
        api.cloudRootServer(),
        api.cloudSession(),
      ]);
      set({ rootServer, session });
      // The TypeScript client shares Rust's cookie jar but not its state:
      // it has to be told which tenant it is talking to and who as.
      adoptSession(session);
    } catch {
      // Runs on every launch, before the user has asked for anything. Failing
      // loudly here would put an error in front of someone who only wanted to
      // open a note; they will see a real one if they try to sign in.
    }
  },

  setRootServer: async (url) => {
    await api.cloudSetRootServer(url);
    // Changing the root changes the tenant, so the session cannot survive it.
    adoptSession(null);
    set({ rootServer: url, session: null });
  },

  signIn: async (strategy, input) => {
    set({ busy: true, error: null });
    try {
      const session = await strategy.login(input);
      adoptSession(session);
      set({ session, busy: false, rootServer: await api.cloudRootServer() });
      return true;
    } catch (err) {
      set({ busy: false, error: err instanceof Error ? err.message : String(err) });
      return false;
    }
  },

  signOut: async () => {
    try {
      await api.cloudLogout();
    } catch {
      // A server that cannot be reached does not stop us signing out locally;
      // Rust drops the session before it tries the network for this reason.
    }
    adoptSession(null);
    set({ session: null });
  },

  clearError: () => set({ error: null }),
}));

export function isSignedIn(): boolean {
  return useAuthStore.getState().session !== null;
}

// A cloud/classbox/classroom call can fail long after sign-in with the one
// error that means the stored session is gone for good (see
// `api.onSessionExpired`'s doc comment). Without this, the UI keeps showing a
// signed-in account that every action rejects, since nothing else ever clears
// `session` back to null.
api.onSessionExpired(() => {
  adoptSession(null);
  useAuthStore.setState({ session: null, error: api.SESSION_EXPIRED_MESSAGE });
});

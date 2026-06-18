import { describe, expect, it } from "vitest";
import {
  BEARER_CREDENTIALS,
  CREDENTIALS,
  OAUTH_CREDENTIALS,
  accountForId,
  credentialById,
  hintForId,
  isKnownAccount,
  pillClass,
  pillFromPresence,
  pillFromVerify,
  pillLabel,
} from "../core/credentials";

/* The credential registry mirrors the Rust allowlist in
 * src-tauri/src/credentials.rs (CONTRACT part A). These pin the ids/accounts/
 * kinds the panel + backend rely on. */

describe("credential registry (static mirror)", () => {
  it("has the exact v1 set in order", () => {
    expect(CREDENTIALS.map((c) => c.id)).toEqual([
      "anthropic",
      "github",
      "slack",
      "google_client_id",
      "google_client_secret",
      "google_workspace",
      "x_client_id",
      "x_client_secret",
      "x_social",
      "linkedin_client_id",
      "linkedin_client_secret",
      "linkedin_social",
      "google_ads_client_id",
      "google_ads_client_secret",
      "google_ads_developer_token",
      "google_ads_customer_id",
      "google_ads_login_customer_id",
      "google_ads",
      "meta_app_id",
      "meta_app_secret",
      "meta_ad_account_id",
      "meta_ads",
      "whoop_client_id",
      "whoop_client_secret",
      "whoop",
      "homeassistant_url",
      "homeassistant_token",
      "plaid_client_id",
      "plaid_secret",
      "plaid_access_token",
      "maps_api_key",
      "hibp_api_key",
      "elevenlabs_api_key",
    ]);
  });

  it("keeps the Anthropic account EXACTLY anthropic_api_key (daemon reads it)", () => {
    expect(accountForId("anthropic")).toBe("anthropic_api_key");
  });

  it("maps every id to its registered account", () => {
    expect(accountForId("github")).toBe("github_pat");
    expect(accountForId("slack")).toBe("slack_bot_token");
    // Google Workspace: two pasted client rows + one connection-STATUS row
    // whose account is the daemon-written refresh token.
    expect(accountForId("google_client_id")).toBe("google_oauth_client_id");
    expect(accountForId("google_client_secret")).toBe(
      "google_oauth_client_secret",
    );
    expect(accountForId("google_workspace")).toBe("google_oauth_refresh_token");
    // X + LinkedIn: two pasted client rows each + one connection-STATUS row
    // whose account is the daemon-written refresh token.
    expect(accountForId("x_client_id")).toBe("x_oauth_client_id");
    expect(accountForId("x_client_secret")).toBe("x_oauth_client_secret");
    expect(accountForId("x_social")).toBe("x_oauth_refresh_token");
    expect(accountForId("linkedin_client_id")).toBe("linkedin_oauth_client_id");
    expect(accountForId("linkedin_client_secret")).toBe(
      "linkedin_oauth_client_secret",
    );
    expect(accountForId("linkedin_social")).toBe(
      "linkedin_oauth_refresh_token",
    );
    // Google Ads: pasted client id/secret + developer token + customer id +
    // optional login customer id, plus a SEPARATE-from-Workspace connection-STATUS
    // row whose account is the daemon-written Ads refresh token.
    expect(accountForId("google_ads_client_id")).toBe("google_ads_client_id");
    expect(accountForId("google_ads_client_secret")).toBe(
      "google_ads_client_secret",
    );
    expect(accountForId("google_ads_developer_token")).toBe(
      "google_ads_developer_token",
    );
    expect(accountForId("google_ads_customer_id")).toBe(
      "google_ads_customer_id",
    );
    expect(accountForId("google_ads_login_customer_id")).toBe(
      "google_ads_login_customer_id",
    );
    expect(accountForId("google_ads")).toBe("google_ads_refresh_token");
    // Meta Ads: pasted app id/secret + ad account id, plus a connection-STATUS
    // row whose account is the daemon-written long-lived token (no refresh token).
    expect(accountForId("meta_app_id")).toBe("meta_app_id");
    expect(accountForId("meta_app_secret")).toBe("meta_app_secret");
    expect(accountForId("meta_ad_account_id")).toBe("meta_ad_account_id");
    expect(accountForId("meta_ads")).toBe("meta_long_lived_token");
    // WHOOP (vitalis, Health & Biometrics): two pasted client rows + one
    // connection-STATUS row whose account is the daemon-written refresh token.
    expect(accountForId("whoop_client_id")).toBe("whoop_oauth_client_id");
    expect(accountForId("whoop_client_secret")).toBe("whoop_oauth_client_secret");
    expect(accountForId("whoop")).toBe("whoop_oauth_refresh_token");
    // Home Assistant (dume): two pasted bearer rows, token-based.
    expect(accountForId("homeassistant_url")).toBe("homeassistant_url");
    expect(accountForId("homeassistant_token")).toBe("homeassistant_token");
    // Plaid (midas, Personal Treasury): three pasted bearer rows — client id,
    // secret, and a linked-institution access token. Token-based, not OAuth; MIDAS
    // reads only — none of these moves money.
    expect(accountForId("plaid_client_id")).toBe("plaid_client_id");
    expect(accountForId("plaid_secret")).toBe("plaid_secret");
    expect(accountForId("plaid_access_token")).toBe("plaid_access_token");
    // Maps (voyager, Travel & Logistics): one pasted bearer row — the Maps Platform
    // API key. Key-based, not OAuth; VOYAGER reads routes/places/times only.
    expect(accountForId("maps_api_key")).toBe("maps_api_key");
    // HIBP (aegis, Defense & Privacy): one pasted bearer row — the Have I Been Pwned
    // API key. Key-based, not OAuth; AEGIS checks the user's OWN email's exposure.
    expect(accountForId("hibp_api_key")).toBe("hibp_api_key");
    // ElevenLabs cloud voice tier: one pasted bearer row — the API key. Key-based,
    // not OAuth; the cloud voice tier ships OFF, Kokoro stays the default + fallback.
    expect(accountForId("elevenlabs_api_key")).toBe("elevenlabs_api_key");
    // The old placeholder rows are gone.
    expect(accountForId("google_drive")).toBeNull();
    expect(accountForId("google_calendar")).toBeNull();
    expect(accountForId("nope")).toBeNull();
  });

  it("labels each credential", () => {
    expect(credentialById("anthropic")?.label).toBe("Anthropic API Key");
    expect(credentialById("github")?.label).toBe("GitHub Token (PAT)");
    expect(credentialById("slack")?.label).toBe("Slack Bot Token");
    expect(credentialById("google_client_id")?.label).toBe(
      "Google OAuth Client ID",
    );
    expect(credentialById("google_client_secret")?.label).toBe(
      "Google OAuth Client Secret",
    );
    expect(credentialById("google_workspace")?.label).toBe("Google Workspace");
    expect(credentialById("x_client_id")?.label).toBe("X (Twitter) Client ID");
    expect(credentialById("x_client_secret")?.label).toBe(
      "X (Twitter) Client Secret",
    );
    expect(credentialById("x_social")?.label).toBe("X (Twitter)");
    expect(credentialById("linkedin_client_id")?.label).toBe(
      "LinkedIn Client ID",
    );
    expect(credentialById("linkedin_client_secret")?.label).toBe(
      "LinkedIn Client Secret",
    );
    expect(credentialById("linkedin_social")?.label).toBe("LinkedIn");
    expect(credentialById("google_ads_client_id")?.label).toBe(
      "Google Ads Client ID",
    );
    expect(credentialById("google_ads_developer_token")?.label).toBe(
      "Google Ads Developer Token",
    );
    expect(credentialById("google_ads_customer_id")?.label).toBe(
      "Google Ads Customer ID",
    );
    expect(credentialById("google_ads")?.label).toBe("Google Ads");
    expect(credentialById("meta_app_id")?.label).toBe("Meta App ID");
    expect(credentialById("meta_ad_account_id")?.label).toBe(
      "Meta Ad Account ID",
    );
    expect(credentialById("meta_ads")?.label).toBe("Meta Ads");
    expect(credentialById("whoop_client_id")?.label).toBe("WHOOP Client ID");
    expect(credentialById("whoop_client_secret")?.label).toBe(
      "WHOOP Client Secret",
    );
    expect(credentialById("whoop")?.label).toBe("WHOOP");
  });

  it("classifies bearer vs oauth kinds", () => {
    expect(BEARER_CREDENTIALS.map((c) => c.id)).toEqual([
      "anthropic",
      "github",
      "slack",
      "google_client_id",
      "google_client_secret",
      "x_client_id",
      "x_client_secret",
      "linkedin_client_id",
      "linkedin_client_secret",
      "google_ads_client_id",
      "google_ads_client_secret",
      "google_ads_developer_token",
      "google_ads_customer_id",
      "google_ads_login_customer_id",
      "meta_app_id",
      "meta_app_secret",
      "meta_ad_account_id",
      "whoop_client_id",
      "whoop_client_secret",
      "homeassistant_url",
      "homeassistant_token",
      "plaid_client_id",
      "plaid_secret",
      "plaid_access_token",
      "maps_api_key",
      "hibp_api_key",
      "elevenlabs_api_key",
    ]);
    // The OAuth rows are the connection STATUS rows (daemon-written refresh /
    // long-lived tokens) — Google Workspace, X, LinkedIn, Google Ads, Meta Ads,
    // WHOOP. Home Assistant and Plaid are token-based (all bearer rows), so they add
    // no OAuth status row.
    expect(OAUTH_CREDENTIALS.map((c) => c.id)).toEqual([
      "google_workspace",
      "x_social",
      "linkedin_social",
      "google_ads",
      "meta_ads",
      "whoop",
    ]);
  });

  it("has unique ids and unique accounts", () => {
    const ids = CREDENTIALS.map((c) => c.id);
    const accts = CREDENTIALS.map((c) => c.keychain_account);
    expect(new Set(ids).size).toBe(ids.length);
    expect(new Set(accts).size).toBe(accts.length);
  });

  it("allowlists only registered accounts (rejects arbitrary writes)", () => {
    expect(isKnownAccount("anthropic_api_key")).toBe(true);
    expect(isKnownAccount("github_pat")).toBe(true);
    expect(isKnownAccount("slack_bot_token")).toBe(true);
    expect(isKnownAccount("google_oauth_client_id")).toBe(true);
    expect(isKnownAccount("google_oauth_client_secret")).toBe(true);
    expect(isKnownAccount("google_oauth_refresh_token")).toBe(true);
    expect(isKnownAccount("x_oauth_client_id")).toBe(true);
    expect(isKnownAccount("x_oauth_client_secret")).toBe(true);
    expect(isKnownAccount("x_oauth_refresh_token")).toBe(true);
    expect(isKnownAccount("linkedin_oauth_client_id")).toBe(true);
    expect(isKnownAccount("linkedin_oauth_client_secret")).toBe(true);
    expect(isKnownAccount("linkedin_oauth_refresh_token")).toBe(true);
    // Google Ads + Meta Ads accounts.
    expect(isKnownAccount("google_ads_client_id")).toBe(true);
    expect(isKnownAccount("google_ads_client_secret")).toBe(true);
    expect(isKnownAccount("google_ads_developer_token")).toBe(true);
    expect(isKnownAccount("google_ads_customer_id")).toBe(true);
    expect(isKnownAccount("google_ads_login_customer_id")).toBe(true);
    expect(isKnownAccount("google_ads_refresh_token")).toBe(true);
    expect(isKnownAccount("meta_app_id")).toBe(true);
    expect(isKnownAccount("meta_app_secret")).toBe(true);
    expect(isKnownAccount("meta_ad_account_id")).toBe(true);
    expect(isKnownAccount("meta_long_lived_token")).toBe(true);
    // WHOOP accounts.
    expect(isKnownAccount("whoop_oauth_client_id")).toBe(true);
    expect(isKnownAccount("whoop_oauth_client_secret")).toBe(true);
    expect(isKnownAccount("whoop_oauth_refresh_token")).toBe(true);
    // Home Assistant accounts (dume).
    expect(isKnownAccount("homeassistant_url")).toBe(true);
    expect(isKnownAccount("homeassistant_token")).toBe(true);
    // Plaid accounts (midas).
    expect(isKnownAccount("plaid_client_id")).toBe(true);
    expect(isKnownAccount("plaid_secret")).toBe(true);
    expect(isKnownAccount("plaid_access_token")).toBe(true);
    // Maps account (voyager).
    expect(isKnownAccount("maps_api_key")).toBe(true);
    // HIBP account (aegis).
    expect(isKnownAccount("hibp_api_key")).toBe(true);
    // ElevenLabs cloud voice tier account.
    expect(isKnownAccount("elevenlabs_api_key")).toBe(true);
    // The retired placeholder accounts are no longer admitted.
    expect(isKnownAccount("google_drive_oauth")).toBe(false);
    expect(isKnownAccount("google_calendar_oauth")).toBe(false);
    expect(isKnownAccount("login_keychain")).toBe(false);
    expect(isKnownAccount("../../evil")).toBe(false);
    expect(isKnownAccount("")).toBe(false);
  });

  it("returns null for an unknown id lookup", () => {
    expect(credentialById("loki")).toBeNull();
    expect(credentialById("")).toBeNull();
  });
});

describe("panel scope hints (UI-only, outside the lockstep registry)", () => {
  it("gives GitHub a repo-scope PAT hint", () => {
    expect(hintForId("github")).toBe(
      "Fine-grained or classic PAT with repo scope",
    );
  });

  it("gives Slack a bot-token + scopes hint", () => {
    expect(hintForId("slack")).toBe(
      "Bot token, xoxb-…, with channels:read + chat:write",
    );
  });

  it("tells the user where the Google client id comes from", () => {
    expect(hintForId("google_client_id")).toContain("Desktop app");
    expect(hintForId("google_client_id")).toContain(
      ".apps.googleusercontent.com",
    );
  });

  it("lists the consent scopes under the Google client secret", () => {
    const hint = hintForId("google_client_secret") ?? "";
    expect(hint).toContain("calendar.events");
    expect(hint).toContain("gmail.readonly");
    expect(hint).toContain("gmail.send");
    expect(hint).toContain("drive.file");
    expect(hint).toContain("drive.metadata.readonly");
  });

  it("tells the user where the X client id comes from (tweet.write)", () => {
    const hint = hintForId("x_client_id") ?? "";
    expect(hint).toContain("Developer Portal");
    expect(hint).toContain("tweet.write");
  });

  it("points the X client secret at the daemon 'connect X' flow", () => {
    expect(hintForId("x_client_secret")).toContain("connect X");
  });

  it("tells the user the LinkedIn app needs the posting product", () => {
    const hint = hintForId("linkedin_client_id") ?? "";
    expect(hint).toContain("Developer Portal");
    expect(hint).toContain("w_member_social");
  });

  it("points the LinkedIn client secret at the daemon 'connect LinkedIn' flow", () => {
    expect(hintForId("linkedin_client_secret")).toContain("connect LinkedIn");
  });

  it("tells the user the Google Ads client id is separate from Workspace", () => {
    const hint = hintForId("google_ads_client_id") ?? "";
    expect(hint).toContain(".apps.googleusercontent.com");
    expect(hint).toContain("Ads");
  });

  it("explains the Google Ads developer token + customer id", () => {
    expect(hintForId("google_ads_developer_token")).toContain("developer-token");
    expect(hintForId("google_ads_customer_id")).toContain("digits only");
  });

  it("points the Meta app secret at the daemon 'connect Meta' flow", () => {
    expect(hintForId("meta_app_secret")).toContain("connect Meta");
  });

  it("tells the user the Meta ad account id format", () => {
    expect(hintForId("meta_ad_account_id")).toContain("act_");
  });

  it("tells the user where the WHOOP client id comes from (read scopes)", () => {
    const hint = hintForId("whoop_client_id") ?? "";
    expect(hint).toContain("Developer Dashboard");
    expect(hint).toContain("read");
  });

  it("points the WHOOP client secret at the daemon 'connect WHOOP' flow", () => {
    expect(hintForId("whoop_client_secret")).toContain("connect WHOOP");
  });

  it("tells the user where the Plaid client id comes from (reads only)", () => {
    const hint = hintForId("plaid_client_id") ?? "";
    expect(hint).toContain("Plaid Dashboard");
    expect(hint.toLowerCase()).toContain("never move");
  });

  it("tells the user the Plaid access token comes from Plaid Link (read-only)", () => {
    const hint = hintForId("plaid_access_token") ?? "";
    expect(hint).toContain("Plaid Link");
    expect(hint.toLowerCase()).toContain("read-only");
  });

  it("tells the user where the Maps key comes from and that VOYAGER reads only", () => {
    const hint = hintForId("maps_api_key") ?? "";
    expect(hint).toContain("Google Maps Platform");
    // Honest scope: reads routes/places/times, never books or pays.
    expect(hint.toLowerCase()).toContain("never books or pays");
  });

  it("tells the user where the HIBP key comes from and that AEGIS is defensive", () => {
    const hint = hintForId("hibp_api_key") ?? "";
    expect(hint).toContain("haveibeenpwned.com");
    // Honest scope: checks the user's OWN email only, never scans anyone else.
    expect(hint.toLowerCase()).toContain("never scans anyone else");
  });

  it("has no hint for status/oauth rows or unknown ids", () => {
    expect(hintForId("anthropic")).toBeNull();
    expect(hintForId("google_workspace")).toBeNull();
    expect(hintForId("x_social")).toBeNull();
    expect(hintForId("linkedin_social")).toBeNull();
    expect(hintForId("google_ads")).toBeNull();
    expect(hintForId("meta_ads")).toBeNull();
    expect(hintForId("whoop")).toBeNull();
    expect(hintForId("nope")).toBeNull();
    expect(hintForId("")).toBeNull();
  });
});

describe("pill mapping (verify result -> pill state)", () => {
  it("valid+stored collapses to ON FILE (learn-green)", () => {
    const p = pillFromVerify({ status: "valid", detail: "octocat", stored: true });
    expect(p.kind).toBe("on_file");
    expect(pillClass(p)).toBe("good");
    expect(pillLabel(p)).toBe("ON FILE");
  });

  it("valid-but-unstored (bare verify) shows the VALID badge with detail", () => {
    const p = pillFromVerify({ status: "valid", detail: "models reachable" });
    expect(p).toEqual({ kind: "valid", detail: "models reachable" });
    expect(pillClass(p)).toBe("good");
    expect(pillLabel(p)).toBe("VALID");
  });

  it("unauthorized -> INVALID (alert-red), never stored", () => {
    const p = pillFromVerify({ status: "unauthorized", detail: "key rejected", stored: false });
    expect(p).toEqual({ kind: "invalid", detail: "key rejected" });
    expect(pillClass(p)).toBe("bad");
    expect(pillLabel(p)).toBe("INVALID");
  });

  it("network_error -> NETWORK ERROR (warn-amber)", () => {
    const p = pillFromVerify({ status: "network_error", detail: "timeout", stored: false });
    expect(p).toEqual({ kind: "network", detail: "timeout" });
    expect(pillClass(p)).toBe("warn");
    expect(pillLabel(p)).toBe("NETWORK ERROR");
  });

  it("presence maps to ON FILE vs empty", () => {
    expect(pillFromPresence(true)).toEqual({ kind: "on_file" });
    expect(pillFromPresence(false)).toEqual({ kind: "empty" });
    expect(pillClass({ kind: "empty" })).toBe("idle");
    expect(pillLabel({ kind: "empty" })).toBe("—");
  });

  it("verifying is the transient idle-coloured badge", () => {
    expect(pillClass({ kind: "verifying" })).toBe("idle");
    expect(pillLabel({ kind: "verifying" })).toBe("VERIFYING…");
  });
});

import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands } from "@/bindings";
import { useSettings } from "../../hooks/useSettings";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";
import { SettingContainer } from "../ui/SettingContainer";
import { ToggleSwitch } from "../ui/ToggleSwitch";

interface RemoteTranscriptionProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

type TestResult =
  | { state: "idle" }
  | { state: "testing" }
  | { state: "ok"; model: string }
  | { state: "error"; message: string };

/** Sends recordings to another Handy's transcription server instead of the
 * local model. */
export const RemoteTranscription: React.FC<RemoteTranscriptionProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const enabled = getSetting("remote_transcription_enabled") ?? false;
  const url = getSetting("remote_transcription_url") ?? "";
  const token = getSetting("remote_transcription_token") ?? "";

  const [urlText, setUrlText] = useState(url);
  const [tokenText, setTokenText] = useState(token);
  const [test, setTest] = useState<TestResult>({ state: "idle" });

  useEffect(() => {
    setUrlText(url);
  }, [url]);

  useEffect(() => {
    setTokenText(token);
  }, [token]);

  const commitUrl = async () => {
    const value = urlText.trim();
    if (value !== url) {
      setTest({ state: "idle" });
      await updateSetting("remote_transcription_url", value);
    }
  };

  const commitToken = async () => {
    const value = tokenText.trim();
    if (value !== token) {
      setTest({ state: "idle" });
      await updateSetting("remote_transcription_token", value);
    }
  };

  const runTest = async () => {
    await commitUrl();
    await commitToken();
    setTest({ state: "testing" });
    const result = await commands.testRemoteTranscription();
    setTest(
      result.status === "ok"
        ? { state: "ok", model: result.data }
        : { state: "error", message: result.error },
    );
  };

  const blurOnEnter = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") {
      event.currentTarget.blur();
    }
  };

  return (
    <>
      <ToggleSwitch
        checked={enabled}
        onChange={(value) =>
          updateSetting("remote_transcription_enabled", value)
        }
        isUpdating={isUpdating("remote_transcription_enabled")}
        label={t("settings.advanced.remoteTranscription.label")}
        description={t("settings.advanced.remoteTranscription.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
      {enabled && (
        <>
          <SettingContainer
            title={t("settings.advanced.remoteTranscription.url.title")}
            description={t(
              "settings.advanced.remoteTranscription.url.description",
            )}
            descriptionMode={descriptionMode}
            grouped={grouped}
            layout="stacked"
          >
            <Input
              type="text"
              value={urlText}
              placeholder="http://192.168.0.21:8765/v1"
              onChange={(event) => setUrlText(event.target.value)}
              onBlur={() => void commitUrl()}
              onKeyDown={blurOnEnter}
              disabled={isUpdating("remote_transcription_url")}
              variant="compact"
              className="w-full font-mono"
            />
          </SettingContainer>
          <SettingContainer
            title={t("settings.advanced.remoteTranscription.token.title")}
            description={t(
              "settings.advanced.remoteTranscription.token.description",
            )}
            descriptionMode={descriptionMode}
            grouped={grouped}
            layout="stacked"
          >
            <Input
              type="password"
              value={tokenText}
              onChange={(event) => setTokenText(event.target.value)}
              onBlur={() => void commitToken()}
              onKeyDown={blurOnEnter}
              disabled={isUpdating("remote_transcription_token")}
              variant="compact"
              className="w-full font-mono"
              autoComplete="off"
            />
          </SettingContainer>
          <SettingContainer
            title={t("settings.advanced.remoteTranscription.test.title")}
            description={t(
              "settings.advanced.remoteTranscription.test.description",
            )}
            descriptionMode={descriptionMode}
            grouped={grouped}
            layout="stacked"
          >
            <div className="flex items-center gap-3">
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void runTest()}
                disabled={test.state === "testing"}
              >
                {test.state === "testing"
                  ? t("settings.advanced.remoteTranscription.test.testing")
                  : t("settings.advanced.remoteTranscription.test.button")}
              </Button>
              {test.state === "ok" && (
                <span className="text-xs text-text break-all">
                  {t("settings.advanced.remoteTranscription.test.connected", {
                    model: test.model,
                  })}
                </span>
              )}
              {test.state === "error" && (
                <span className="text-xs text-error break-words">
                  {test.message}
                </span>
              )}
            </div>
          </SettingContainer>
        </>
      )}
    </>
  );
};

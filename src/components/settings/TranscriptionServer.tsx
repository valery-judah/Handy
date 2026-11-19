import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { commands, type TranscriptionServerStatus } from "@/bindings";
import { notify } from "@/stores/noticeStore";
import { useSettings } from "../../hooks/useSettings";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";
import { SettingContainer } from "../ui/SettingContainer";
import { TextDisplay } from "../ui/TextDisplay";
import { ToggleSwitch } from "../ui/ToggleSwitch";

interface TranscriptionServerProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/** Turns the transcription server on and shows what a client needs to
 * connect: the URL and the token. */
export const TranscriptionServer: React.FC<TranscriptionServerProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating, refreshSettings } =
    useSettings();

  const enabled = getSetting("transcription_server_enabled") ?? false;
  const port = getSetting("transcription_server_port") ?? 8765;
  const token = getSetting("transcription_server_token") ?? "";

  const [status, setStatus] = useState<TranscriptionServerStatus | null>(null);
  const [portText, setPortText] = useState(String(port));
  const [ownToken, setOwnToken] = useState("");
  const [savingToken, setSavingToken] = useState(false);

  useEffect(() => {
    setPortText(String(port));
  }, [port]);

  const loadStatus = useCallback(async () => {
    setStatus(await commands.getTranscriptionServerStatus());
  }, []);

  useEffect(() => {
    void loadStatus();
  }, [enabled, port, loadStatus]);

  const handleToggle = async (value: boolean) => {
    await updateSetting("transcription_server_enabled", value);
    // Turning it on the first time creates the token.
    await refreshSettings();
    await loadStatus();
  };

  const commitPort = async () => {
    const value = parseInt(portText, 10);
    if (isNaN(value) || value === port) {
      setPortText(String(port));
      return;
    }
    await updateSetting("transcription_server_port", value);
    await refreshSettings();
    await loadStatus();
  };

  const regenerateToken = async () => {
    const result = await commands.regenerateTranscriptionServerToken();
    if (result.status === "error") {
      notify({ title: result.error });
    }
    await refreshSettings();
  };

  const saveOwnToken = async () => {
    setSavingToken(true);
    const result =
      await commands.changeTranscriptionServerTokenSetting(ownToken);
    setSavingToken(false);
    if (result.status === "error") {
      notify({ title: result.error });
      return;
    }
    setOwnToken("");
    await refreshSettings();
  };

  return (
    <>
      <ToggleSwitch
        checked={enabled}
        onChange={handleToggle}
        isUpdating={isUpdating("transcription_server_enabled")}
        label={t("settings.advanced.transcriptionServer.label")}
        description={t("settings.advanced.transcriptionServer.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
      {enabled && (
        <>
          <SettingContainer
            title={t("settings.advanced.transcriptionServer.port.title")}
            description={t(
              "settings.advanced.transcriptionServer.port.description",
            )}
            descriptionMode={descriptionMode}
            grouped={grouped}
            layout="horizontal"
          >
            <Input
              type="number"
              min="1024"
              max="65535"
              value={portText}
              onChange={(event) => setPortText(event.target.value)}
              onBlur={() => void commitPort()}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.currentTarget.blur();
                }
              }}
              disabled={isUpdating("transcription_server_port")}
              className="w-24"
            />
          </SettingContainer>
          <TextDisplay
            label={t("settings.advanced.transcriptionServer.url.title")}
            description={t(
              "settings.advanced.transcriptionServer.url.description",
            )}
            value={status?.url ?? ""}
            placeholder={t("settings.advanced.transcriptionServer.notRunning")}
            descriptionMode={descriptionMode}
            grouped={grouped}
            copyable
            monospace
          />
          <TextDisplay
            label={t("settings.advanced.transcriptionServer.token.title")}
            description={t(
              "settings.advanced.transcriptionServer.token.description",
            )}
            value={token}
            descriptionMode={descriptionMode}
            grouped={grouped}
            copyable
            monospace
          />
          <SettingContainer
            title={t("settings.advanced.transcriptionServer.ownToken.title")}
            description={t(
              "settings.advanced.transcriptionServer.ownToken.description",
            )}
            descriptionMode={descriptionMode}
            grouped={grouped}
            layout="stacked"
          >
            <div className="flex items-center gap-2">
              <Input
                type="password"
                value={ownToken}
                onChange={(event) => setOwnToken(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && ownToken.trim()) {
                    void saveOwnToken();
                  }
                }}
                variant="compact"
                className="flex-1 min-w-0 font-mono"
                autoComplete="off"
              />
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void saveOwnToken()}
                disabled={savingToken || !ownToken.trim()}
              >
                {t("settings.advanced.transcriptionServer.ownToken.button")}
              </Button>
            </div>
          </SettingContainer>
          <SettingContainer
            title={t("settings.advanced.transcriptionServer.regenerate.title")}
            description={t(
              "settings.advanced.transcriptionServer.regenerate.description",
            )}
            descriptionMode={descriptionMode}
            grouped={grouped}
            layout="horizontal"
          >
            <Button
              variant="secondary"
              size="sm"
              onClick={() => void regenerateToken()}
            >
              {t("settings.advanced.transcriptionServer.regenerate.button")}
            </Button>
          </SettingContainer>
        </>
      )}
    </>
  );
};

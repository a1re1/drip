// Bottom of the stream: one glass composer for every operator input. With a
// session selected the text lands in its inbox (`drip --send`) — a running
// goal picks it up at its next cycle boundary, otherwise the server resumes
// the session. With no session selected it starts a new detached run.
import { useState } from "react";
import { Icon, IconButton } from "./icons";

export type ComposerMode = "new" | "running" | "idle";

interface Props {
  mode: ComposerMode;
  disabled: boolean;
  /** Resolves true when the text was accepted; the draft is kept otherwise. */
  onSubmit: (text: string, maxIterations?: number) => Promise<boolean>;
  onStop: () => void;
}

const PLACEHOLDER: Record<ComposerMode, string> = {
  new: "Describe the goal for a new session",
  running: "Message the running agent",
  idle: "Resume this session with a prompt",
};

export function Composer({ mode, disabled, onSubmit, onStop }: Props) {
  const [text, setText] = useState("");
  const [maxIterations, setMaxIterations] = useState("");
  const hasDraft = text.trim() !== "";

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || disabled) return;
    const limit = Number.parseInt(maxIterations, 10);
    const accepted = await onSubmit(trimmed, mode === "new" && Number.isInteger(limit) && limit > 0 ? limit : undefined);
    if (accepted) setText("");
  };

  const rows = Math.min(6, Math.max(1, text.split("\n").length));

  return (
    <form
      className="flex justify-center"
      style={{ padding: "6px 48px 14px" }}
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <div className="composer flex flex-col" style={{ width: "min(100%, 760px)", gap: 6, padding: "12px 12px 10px" }}>
        <textarea
          value={text}
          onChange={(event) => setText(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              void submit();
            }
          }}
          disabled={disabled}
          placeholder={PLACEHOLDER[mode]}
          rows={rows}
          className="t-callout w-full resize-none border-0 bg-transparent outline-none disabled:opacity-50"
          style={{ lineHeight: 1.5, padding: "2px 4px", maxHeight: 200, color: "var(--text-primary)" }}
        />
        <div className="flex flex-wrap items-center gap-1" style={{ minHeight: 28 }}>
          <span className="vt-btn vt-btn--muted vt-btn--s" style={{ cursor: "default" }}>
            <Icon name={mode === "new" ? "plus" : mode === "running" ? "clock" : "play"} size={13} />
            {mode === "new" ? "New session" : mode === "running" ? "Next cycle boundary" : "Resume session"}
          </span>
          {mode === "new" && (
            <label className="vt-input t-footnote" style={{ height: 24, width: 120 }}>
              <span className="c-tertiary whitespace-nowrap">max iters</span>
              <input
                value={maxIterations}
                onChange={(event) => setMaxIterations(event.target.value.replace(/[^0-9]/g, ""))}
                placeholder="∞"
                inputMode="numeric"
                className="t-footnote"
                style={{ width: 40 }}
              />
            </label>
          )}
          <span className="ml-auto" />
          <span className="t-caption c-tertiary whitespace-nowrap" style={{ padding: "0 4px" }}>
            {hasDraft ? `${text.length} chars · Enter to send` : "Shift+Enter for a newline"}
          </span>
          {mode === "running" && !hasDraft ? (
            <IconButton icon="pause" label="Stop agent" variant="glass" size="m" disabled={disabled} onClick={onStop} />
          ) : (
            <IconButton
              icon={mode === "idle" || mode === "new" ? "play" : "send"}
              label={mode === "new" ? "Start session" : mode === "idle" ? "Resume" : "Send"}
              variant={hasDraft ? "primary" : "glass"}
              size="m"
              type="submit"
              disabled={disabled || !hasDraft}
            />
          )}
        </div>
      </div>
    </form>
  );
}

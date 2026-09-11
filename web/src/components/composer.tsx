// Bottom of the timeline: operator input. The text always lands in the
// session inbox (`drip --send`); a running goal picks it up at its next
// cycle boundary, otherwise the server resumes the session and the new run
// consumes it.
import { useState } from "react";

interface Props {
  disabled: boolean;
  isRunning: boolean;
  /** Resolves true when the message was accepted; the draft is kept otherwise. */
  onSubmit: (text: string) => Promise<boolean>;
  onStop: () => void;
}

export function Composer({ disabled, isRunning, onSubmit, onStop }: Props) {
  const [text, setText] = useState("");

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || disabled) return;
    if (await onSubmit(trimmed)) setText("");
  };

  return (
    <form
      className="border-t border-neutral-800 p-3"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
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
        placeholder={isRunning ? "Message the running agent (lands at the next cycle boundary)…" : "Resume this session with a prompt…"}
        rows={3}
        className="w-full resize-none rounded-md border border-neutral-800 bg-neutral-900 px-3 py-2 text-sm text-neutral-100 outline-none placeholder:text-neutral-600 focus:border-neutral-600 disabled:opacity-50"
      />
      <div className="mt-2 flex items-center gap-2">
        <span className="text-[11px] text-neutral-600">Enter to send · Shift+Enter for a newline</span>
        {isRunning && (
          <button
            type="button"
            onClick={onStop}
            disabled={disabled}
            className="ml-auto rounded-md border border-red-800 px-3 py-1 text-xs text-red-300 disabled:opacity-40"
          >
            Stop
          </button>
        )}
        <button
          type="submit"
          disabled={disabled || text.trim() === ""}
          className={`${isRunning ? "" : "ml-auto"} rounded-md bg-neutral-100 px-3 py-1 text-xs font-medium text-neutral-900 disabled:opacity-40`}
        >
          {isRunning ? "Send" : "Resume"}
        </button>
      </div>
    </form>
  );
}

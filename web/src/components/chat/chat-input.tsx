/**
 * Chat message input with send button.
 */
import { useCallback, useRef } from 'react';
import { Send } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { useTimelineStore } from '@/stores/timeline-store';

interface Props {
  value: string;
  onChange: (value: string) => void;
}

export function ChatInput({ value, onChange }: Props) {
  const threadId = useTimelineStore((s) => s.threadId);
  const loading = useTimelineStore((s) => s.loading);
  const sendMessage = useTimelineStore((s) => s.sendMessage);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  const handleSend = useCallback(() => {
    if (!value.trim()) return;
    void sendMessage(value.trim());
    onChange('');
  }, [value, onChange, sendMessage]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
        e.preventDefault();
        handleSend();
      }
    },
    [handleSend],
  );

  return (
    <footer className="glass sticky bottom-0 z-10 px-4 py-3 md:px-6">
      <div className="mx-auto flex max-w-3xl gap-2">
        <Textarea
          ref={textareaRef}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={
            threadId
              ? 'Type a message... (Enter to send)'
              : 'Create a thread first'
          }
          disabled={!threadId || loading}
          rows={1}
          className="max-h-32 min-h-0 resize-none rounded-xl bg-background/60 py-2.5 backdrop-blur-sm transition-all duration-200 focus:ring-2 focus:ring-primary/30"
        />
        <Button
          size="icon"
          className="h-11 w-11 shrink-0 rounded-xl transition-transform duration-200 hover:scale-105 active:scale-95"
          disabled={!threadId || !value.trim() || loading}
          onClick={handleSend}
        >
          <Send className="h-4 w-4" />
        </Button>
      </div>
    </footer>
  );
}

import { useEffect, useState } from 'react';
import { TooltipProvider } from '@/components/ui/tooltip';
import { ChatHeader } from '@/components/chat/chat-header';
import { ChatTimeline } from '@/components/chat/chat-timeline';
import { ChatInput } from '@/components/chat/chat-input';
import { ThreadSidebar } from '@/components/chat/thread-sidebar';
import { useCodexSocket } from '@/hooks/use-codex-socket';

function App() {
  const [input, setInput] = useState('');
  const [dark, setDark] = useState(() =>
    window.matchMedia('(prefers-color-scheme: dark)').matches,
  );

  useCodexSocket();

  useEffect(() => {
    document.documentElement.classList.toggle('dark', dark);
  }, [dark]);

  return (
    <TooltipProvider>
      <div className="flex h-dvh overflow-hidden bg-background">
        <ThreadSidebar />
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <ChatHeader dark={dark} onToggleDark={() => setDark((d) => !d)} />
          <ChatTimeline />
          <ChatInput value={input} onChange={setInput} />
        </div>
      </div>
    </TooltipProvider>
  );
}

export default App;

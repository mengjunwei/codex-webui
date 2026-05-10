import { useEffect, useState } from 'react';
import { TooltipProvider } from '@/components/ui/tooltip';
import { ChatHeader } from '@/components/chat/chat-header';
import { ChatTimeline } from '@/components/chat/chat-timeline';
import { ChatInput } from '@/components/chat/chat-input';
import { ThreadSidebar } from '@/components/chat/thread-sidebar';
import { FilesPanel } from '@/components/files/files-panel';
import { useCodexSocket } from '@/hooks/use-codex-socket';
import { useFilesStore } from '@/stores/files-store';

function App() {
  const [input, setInput] = useState('');
  const [dark, setDark] = useState(() =>
    window.matchMedia('(prefers-color-scheme: dark)').matches,
  );
  const panelOpen = useFilesStore((s) => s.panelOpen);

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
          {panelOpen ? (
            <FilesPanel />
          ) : (
            <>
              <ChatTimeline />
              <ChatInput value={input} onChange={setInput} />
            </>
          )}
        </div>
      </div>
    </TooltipProvider>
  );
}

export default App;

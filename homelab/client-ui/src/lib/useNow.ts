import { useEffect, useState } from "react";

export function useNowSecs(everyMs = 60_000): number {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now() / 1000), everyMs);
    return () => clearInterval(id);
  }, [everyMs]);
  return now;
}

import { useEffect, useRef } from 'react';

export default function RobotIcon({ variant = 'portrait' }) {
  const host = useRef(null);
  useEffect(() => {
    let cancelled = false;
    let dispose;
    import('./robotScene.js').then(({ mountRobot }) => {
      if (!cancelled) {
        try { dispose = mountRobot(host.current, { variant }); } catch { /* Keep the scene background when WebGL is unavailable. */ }
      }
    }).catch(() => {});
    return () => { cancelled = true; dispose?.(); };
  }, [variant]);
  return <span ref={host} className={`robot-icon robot-icon--${variant}`} aria-hidden="true" />;
}

import { appendFile, readdir, stat, unlink } from "node:fs/promises";
import { join } from "node:path";
import { Writable } from "node:stream";

const dailyName = /^services-(\d{4}-\d{2}-\d{2})\.log$/;

export function retentionDays(value = process.env.MNEMOARC_LOG_RETENTION_DAYS) {
  const days = value === undefined ? 3 : Number(value);
  if (!Number.isInteger(days) || days < 1 || days > 3650)
    throw new Error(
      "MNEMOARC_LOG_RETENTION_DAYS must be an integer from 1 to 3650.",
    );
  return days;
}

export function localDay(date) {
  const year = String(date.getFullYear()).padStart(4, "0");
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

export function dailyLogPath(directory, date = new Date()) {
  return join(directory, `services-${localDay(date)}.log`);
}

export async function pruneLogs(directory, days = 3, now = new Date()) {
  const cutoff = new Date(
    now.getFullYear(),
    now.getMonth(),
    now.getDate() - days + 1,
  );
  const firstKeptDay = localDay(cutoff);
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (!entry.isFile()) continue;
    const match = dailyName.exec(entry.name);
    if (match) {
      if (match[1] < firstKeptDay) await unlink(join(directory, entry.name));
    } else if (entry.name === "services.log") {
      // The pre-rotation log has no date in its name. Retain recent legacy
      // output, but do not let it evade the same expiration rule.
      const modified = (await stat(join(directory, entry.name))).mtime;
      if (localDay(modified) < firstKeptDay)
        await unlink(join(directory, entry.name));
    }
  }
}

export class DailyLogSink extends Writable {
  constructor(directory, now = () => new Date()) {
    super();
    this.directory = directory;
    this.now = now;
  }

  _write(chunk, _encoding, callback) {
    appendFile(dailyLogPath(this.directory, this.now()), chunk, {
      mode: 0o600,
    }).then(() => callback(), callback);
  }
}

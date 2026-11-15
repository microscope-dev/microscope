


interface Entry {
  readonly amount: number;
}

export function total(entries: number[], checkpoint: () => void): number {
  const sum: number = entries.reduce((sum: number, entry: number) => sum + entry, 0);
  const entry: Entry = {amount: sum};
  checkpoint();
  return entry.amount;
}

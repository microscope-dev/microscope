// TypeScript that the source map tests run through tsx and esbuild, whose
// JavaScript has other lines than these: the types and the enum below
// move every statement.
interface Point {
  x: number;
  y: number;
}

enum Color {
  Red,
  Green,
}

const origin: Point = { x: 3, y: 4 }; // TOP

function norm(p: Point): number {
  const squared: number = p.x * p.x + p.y * p.y; // NORM
  return Math.sqrt(squared); // RETURN
}

function fail(reason: string): never {
  throw new Error(reason); // THROW
}

console.log('NORM', norm(origin), Color.Green);
try {
  fail('mapped');
} catch (error) {
  console.log('CAUGHT', (error as Error).message);
}

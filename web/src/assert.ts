// Checks a fact about the page's own state that the types can't prove. A
// failure is a bug in the page, never bad input from the server, the user, or
// a visualizer. See dev/assertions.md.
export function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(`assertion failed: ${message}`);
  }
}

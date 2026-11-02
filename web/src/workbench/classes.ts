// A CSS module's class names by name. A module's names are not typed, so a
// misspelled one fails here the first time it renders, rather than quietly
// giving the element the class `undefined`.

import { assert } from "../assert";

export function classOf(module: CSSModuleClasses): (name: string) => string {
  return (name) => {
    const found = module[name];
    assert(found !== undefined, `the CSS module has no class ${name}`);
    return found;
  };
}

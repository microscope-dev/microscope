import { assert } from "../assert";

export function classOf(module: CSSModuleClasses): (name: string) => string {
  return (name) => {
    const found = module[name];
    assert(found !== undefined, `the CSS module has no class ${name}`);
    return found;
  };
}

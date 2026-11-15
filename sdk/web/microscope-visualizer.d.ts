






declare namespace microscope {

  type Value =
    | null
    | boolean
    | number
    | bigint
    | string
    | Value[]
    | Int8Array
    | Uint8Array
    | Int16Array
    | Uint16Array
    | Int32Array
    | Uint32Array
    | BigInt64Array
    | BigUint64Array
    | Float32Array
    | Float64Array
    | { [member: string]: Value };


  interface Enumerator {
    name: string | null;
    value: number | bigint;
  }


  interface Variant {
    variant: string;
    value?: Value;
  }


  type Inputs = Record<string, any>;

  interface Context<I = Inputs> {

    previous: I | null;



    paths: { readonly [K in keyof I]: string | null };

    width: number;
    theme: "light" | "dark";
  }


  type Color = string;


  interface Style {
    fill?: Color;
    stroke?: Color;
    strokeWidth?: number;
    opacity?: number;

    dash?: number[];


    title?: string;



    select?: string;
  }

  interface Rect extends Style {
    x: number;
    y: number;
    width: number;
    height: number;
    radius?: number;
  }
  interface Circle extends Style {
    x: number;
    y: number;
    r: number;
  }
  interface Line extends Style {
    x1: number;
    y1: number;
    x2: number;
    y2: number;
  }
  interface Points extends Style {

    points: ArrayLike<number>;
  }
  interface Path extends Style {

    d: string;
  }
  interface Text extends Style {
    x: number;
    y: number;
    text: string;
    size?: number;
    weight?: "normal" | "bold" | number;
    family?: "sans" | "mono";
    anchor?: "start" | "middle" | "end";
    baseline?: "auto" | "alphabetic" | "middle" | "central" | "hanging" | "ideographic";
  }
  interface Group extends Style {
    shapes: Shape[];
    x?: number;
    y?: number;
    scale?: number;

    rotate?: number;
  }
  interface Pixels extends Style {
    x: number;
    y: number;
    width: number;
    height: number;

    pixels: Uint8ClampedArray | Uint8Array;
    columns: number;
    rows: number;

    smooth?: boolean;
  }
  interface Canvas extends Style {
    x: number;
    y: number;
    width: number;
    height: number;

    canvas: OffscreenCanvas;
    smooth?: boolean;
  }


  interface Shape {
    readonly type: string;
  }

  interface Picture {
    readonly type: "picture";
  }


  interface Theme {
    ink: string;
    ink2: string;
    ink3: string;
    paper: string;
    surface: string;
    line: string;
    accent: string;
    changed: string;
    good: string;
    bad: string;

    series: readonly string[];
  }


  function draw<I = Inputs>(
    render: (input: I, context: Context<I>) => Picture | Promise<Picture>,
  ): void;


  interface LiveContext<I = Inputs> {
    previous: I | null;
    paths: { readonly [K in keyof I]: string | null };


    width: number;
    height: number;
    theme: "light" | "dark";
  }


  interface Pointer {
    readonly type: "down" | "move" | "up" | "wheel" | "leave";
    readonly x: number;
    readonly y: number;

    readonly dx: number;
    readonly dy: number;

    readonly buttons: number;

    readonly wheel: number;
    readonly shift: boolean;
    readonly ctrl: boolean;
    readonly alt: boolean;
  }


  interface Key {

    readonly key: string;
    readonly shift: boolean;
    readonly ctrl: boolean;
    readonly alt: boolean;
  }



  interface Live<I = Inputs> {
    frame?(time: number): void | Promise<void>;

    update?(input: I, context: LiveContext<I>): void | Promise<void>;
    pointer?(event: Pointer): void | Promise<void>;
    key?(event: Key): void | Promise<void>;

    resize?(width: number, height: number): void | Promise<void>;
  }



  function live<I = Inputs>(
    start: (
      canvas: OffscreenCanvas,
      input: I,
      context: LiveContext<I>,
    ) => Live<I> | undefined | Promise<Live<I> | undefined>,
  ): void;




  function redraw(): void;

  function animate(on: boolean): void;

  function caption(text: string): void;

  function hint(text: string | null): void;

  function select(path: string): void;

  function picture(picture: {
    width: number;
    height: number;
    shapes: Shape[];
    caption?: string;
  }): Picture;
  function rect(rect: Rect): Shape;
  function circle(circle: Circle): Shape;
  function line(line: Line): Shape;
  function polyline(polyline: Points): Shape;
  function polygon(polygon: Points): Shape;
  function path(path: Path): Shape;
  function text(text: Text): Shape;
  function group(group: Group): Shape;
  function image(image: Pixels | Canvas): Shape;


  const theme: Theme;

  function same(a: unknown, b: unknown): boolean;
  const color: {

    scale(t: number, from: string, to: string): string;
  };
}

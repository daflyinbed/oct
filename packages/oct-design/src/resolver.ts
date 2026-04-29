import type { ComponentInfo, ComponentResolver } from "unplugin-vue-components";

export interface ODesignResolverOptions {
  prefix?: string;
  importStyle?: boolean;
}

function kebabCase(str: string): string {
  return str.replaceAll(/([a-z])([A-Z])/g, "$1-$2").toLowerCase();
}

export function ODesignResolver(
  options: ODesignResolverOptions = {},
): ComponentResolver {
  const { prefix = "O", importStyle = true } = options;

  return {
    type: "component",
    resolve: (name: string): ComponentInfo | undefined => {
      if (!name.startsWith(prefix)) return;

      const partialName = name.slice(prefix.length);
      if (!partialName) return;

      const kebabName = kebabCase(partialName);

      return {
        name: `${prefix}${partialName}`,
        from: "oct-design/es",
        sideEffects: importStyle
          ? [`oct-design/es/components/${kebabName}/style.mjs`]
          : undefined,
      };
    },
  };
}

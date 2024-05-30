import path from "path";
import { Context, changeSpinner, logError } from "../../bundler/context.js";
import { configFromProjectConfig, readProjectConfig } from "./config.js";
import {
  AppDefinitionSpec,
  ComponentDefinitionSpec,
  finishPush,
  startPush,
} from "./deploy2.js";
import { version } from "../version.js";
import { PushOptions } from "./push.js";
import { ensureHasConvexDependency, functionsDir } from "./utils.js";
import {
  bundleDefinitions,
  bundleImplementations,
  componentGraph,
  // TODO probably just a
} from "./components/definition/bundle.js";
import { isComponentDirectory } from "./components/definition/directoryStructure.js";

export async function runComponentsPush(ctx: Context, options: PushOptions) {
  const { configPath, projectConfig } = await readProjectConfig(ctx);
  const verbose = options.verbose || options.dryRun;
  await ensureHasConvexDependency(ctx, "push");

  if (!options.codegen) {
    logError(ctx, "disabling codegen not allowed");
    await ctx.crash(1, "fatal");
  }
  if (options.dryRun) {
    logError(ctx, "dryRun not allowed yet");
    await ctx.crash(1, "fatal");
  }
  if (options.debugBundlePath) {
    logError(ctx, "debugBundlePath not allowed yet");
    await ctx.crash(1, "fatal");
  }
  if (!options.enableComponents) {
    logError(ctx, "enableComponents must be true");
    await ctx.crash(1, "fatal");
  }

  const convexDir = functionsDir(configPath, projectConfig);

  // TODO
  // Note we need to restart this whole process if any of these files change!
  // We should track these separately if we can: if any thing observed
  // by the definition traversal changes we need to restart at an earlier
  // point than if something in one of the implementations changes.
  // If one of the implementions changes we should be able to just rebuild that.

  // '.' means use the process current working directory, it's the default behavior.
  // Spelling it out here to be explicit for a future where this code can run
  // from other directories.
  // In esbuild the working directory is used to print error messages and resolving
  // relatives paths passed to it. It generally doesn't matter for resolving imports,
  // imports are resolved from the file where they are written.
  const absWorkingDir = path.resolve(".");
  const isComponent = isComponentDirectory(ctx, convexDir, true);
  if (isComponent.kind === "err") {
    logError(
      ctx,
      `Invalid component root directory (${isComponent.why}): ${convexDir}`,
    );
    return await ctx.crash(1, "invalid filesystem data");
  }
  const rootComponent = isComponent.component;

  changeSpinner(ctx, "Traversing component definitions...");
  // Create a list of relevant component directories. These are just for knowing
  // while directories to bundle in bundleDefinitions and bundleImplementations.
  // This produces a bundle in memory as a side effect but it's thrown away.
  //
  // This is the very first time we traverse the component graph.
  // We're just traversing to discover
  const { components, dependencyGraph } = await componentGraph(
    ctx,
    absWorkingDir,
    rootComponent,
    verbose,
  );

  changeSpinner(ctx, "Bundling component definitions...");
  // This bundles everything but the actual function definitions
  const {
    appDefinitionSpecWithoutImpls,
    componentDefinitionSpecsWithoutImpls,
  } = await bundleDefinitions(
    ctx,
    absWorkingDir,
    dependencyGraph,
    rootComponent,
    // note this *includes* the root component (TODO update bundleImpls to work this way too)
    [...components.values()],
  );

  // Is this possible to run in this world?
  // Note that it bundles!!! That's a step we don't need.
  const { config: localConfig } = await configFromProjectConfig(
    ctx,
    projectConfig,
    configPath,
    verbose,
  );
  changeSpinner(ctx, "Bundling component schemas and implementations...");
  const { appImplementation, componentImplementations } =
    await bundleImplementations(
      ctx,
      rootComponent,
      [...components.values()],
      verbose,
    );

  // This expects an auth (get it the normal way)
  // and functions which are pretty normal?
  const appDefinition: AppDefinitionSpec = {
    ...appDefinitionSpecWithoutImpls,
    auth: localConfig.authConfig || null,
    ...appImplementation,
  };

  const componentDefinitions: ComponentDefinitionSpec[] = [];
  for (const componentDefinition of componentDefinitionSpecsWithoutImpls) {
    const impl = componentImplementations.filter(
      (impl) =>
        // convert from ComponentPath
        path.resolve(rootComponent.path, impl.definitionPath) ===
        componentDefinition.definitionPath,
    )[0];
    if (!impl) {
      console.log(
        `missing! couldn't find ${componentDefinition.definitionPath} in ${componentImplementations.map((impl) => path.resolve(rootComponent.path, impl.definitionPath)).toString()}`,
      );
      return await ctx.crash(1, "fatal");
    }
    componentDefinitions.push({
      ...componentDefinition,
      ...impl,
    });
  }

  // We're just using the version this CLI is running with for now.
  // This could be different than the version of `convex` the app runs with
  // if the CLI is installed globally.
  const udfServerVersion = version;

  const startPushResponse = await startPush(
    ctx,
    options.adminKey,
    options.url,
    projectConfig.functions, // this is where the convex folder is, just 'convex/'
    udfServerVersion, // this comes from config?
    appDefinition,
    componentDefinitions,
  );

  console.log("startPush:", startPushResponse);

  const finishPushResponse = await finishPush(
    ctx,
    options.adminKey,
    options.url,
    startPushResponse,
  );
  console.log("finishPush:", finishPushResponse);

  // TODO
  // How to make this re-entrant? If there's a change you should be able to stop the current
  // deploy and restart. If one component deep in the chain changes, you should be able.
  // Cached results should be able to be used.
}

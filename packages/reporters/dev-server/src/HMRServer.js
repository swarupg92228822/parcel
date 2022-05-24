// @flow

import type {
  BuildSuccessEvent,
  Dependency,
  PluginOptions,
  BundleGraph,
  PackagedBundle,
  Asset,
} from '@parcel/types';
import type {Diagnostic} from '@parcel/diagnostic';
import type {AnsiDiagnosticResult, createHTTPServer} from '@parcel/utils';
import type {
  ServerError,
  HMRServerOptions,
  Request,
  Response,
} from './types.js.flow';
import {setHeaders, SOURCES_ENDPOINT} from './Server';

import WebSocket from 'ws';
import invariant from 'assert';
import connect from 'connect';
import {ansiHtml, prettyDiagnostic, PromiseQueue} from '@parcel/utils';

export type HMRAsset = {|
  id: string,
  url: string,
  type: string,
  output: string,
  envHash: string,
  depsByBundle: {[string]: {[string]: string, ...}, ...},
|};

export type HMRMessage =
  | {|
      type: 'update',
      assets: Array<HMRAsset>,
    |}
  | {|
      type: 'error',
      diagnostics: {|
        ansi: Array<AnsiDiagnosticResult>,
        html: Array<$Rest<AnsiDiagnosticResult, {|codeframe: string|}>>,
      |},
    |};

const FS_CONCURRENCY = 64;
const HMR_ENDPOINT = '/__parcel_hmr';

export default class HMRServer {
  wss: WebSocket.Server;
  unresolvedError: HMRMessage | null = null;
  options: HMRServerOptions;
  bundleGraph: BundleGraph<PackagedBundle> | null = null;
  stopServer: ?() => Promise<void>;

  constructor(options: HMRServerOptions) {
    this.options = options;
  }

  start(): void {
    let server = this.options.devServer;
    if (!server) {
      let result = createHTTPServer({
        listener: (req, res) => {
          setHeaders(res);
          if (!this.handle(req, res)) {
            res.statusCode = 404;
            res.end();
          }
        },
      });
      server = result.server;
      server.listen(this.options.port, this.options.host);
      this.stopServer = result.stop;
    } else {
      this.options.addMiddleware(this.handle.bind(this));
    }
    this.wss = new WebSocket.Server(server);

    this.wss.on('connection', ws => {
      if (this.unresolvedError) {
        ws.send(JSON.stringify(this.unresolvedError));
      }
    });

    // $FlowFixMe[incompatible-exact]
    this.wss.on('error', err => this.handleSocketError(err));
  }

  handle(req: Request, res: Response): boolean {
    let {pathname} = url.parse(req.originalUrl || req.url);
    if (pathname != null && pathname.startsWith(HMR_ENDPOINT)) {
      let id = pathname.slice(HMR_ENDPOINT.length + 1);
      let bundleGraph = nullthrows(this.bundleGraph);
      let asset = bundleGraph.getAssetById(id);
      let output = await this.getHotAssetContents(asset);

      res.setHeader('Content-Type', mime.contentType(asset.type));
      res.end(output);
      return true;
    }
    return false;
  }

  async stop() {
    if (this.stopServer != null) {
      await this.stopServer();
      this.stopServer = null;
    }
    this.wss.close();
  }

  async emitError(options: PluginOptions, diagnostics: Array<Diagnostic>) {
    let renderedDiagnostics = await Promise.all(
      diagnostics.map(d => prettyDiagnostic(d, options)),
    );

    // store the most recent error so we can notify new connections
    // and so we can broadcast when the error is resolved
    this.unresolvedError = {
      type: 'error',
      diagnostics: {
        ansi: renderedDiagnostics,
        html: renderedDiagnostics.map((d, i) => {
          return {
            message: ansiHtml(d.message),
            stack: ansiHtml(d.stack),
            frames: d.frames.map(f => ({
              location: f.location,
              code: ansiHtml(f.code),
            })),
            hints: d.hints.map(hint => ansiHtml(hint)),
            documentation: diagnostics[i].documentationURL ?? '',
          };
        }),
      },
    };

    this.broadcast(this.unresolvedError);
  }

  async emitUpdate(event: BuildSuccessEvent) {
    this.unresolvedError = null;
    this.bundleGraph = event.bundleGraph;

    let changedAssets = new Set(event.changedAssets.values());
    if (changedAssets.size === 0) return;

    let queue = new PromiseQueue({maxConcurrent: FS_CONCURRENCY});
    for (let asset of changedAssets) {
      if (asset.type !== 'js' && asset.type !== 'css') {
        // If all of the incoming dependencies of the asset actually resolve to a JS asset
        // rather than the original, we can mark the runtimes as changed instead. URL runtimes
        // have a cache busting query param added with HMR enabled which will trigger a reload.
        let runtimes = new Set();
        let incomingDeps = event.bundleGraph.getIncomingDependencies(asset);
        let isOnlyReferencedByRuntimes = incomingDeps.every(dep => {
          let resolved = event.bundleGraph.getResolvedAsset(dep);
          let isRuntime = resolved?.type === 'js' && resolved !== asset;
          if (resolved && isRuntime) {
            runtimes.add(resolved);
          }
          return isRuntime;
        });

        if (isOnlyReferencedByRuntimes) {
          for (let runtime of runtimes) {
            changedAssets.add(runtime);
          }

          continue;
        }
      }

      queue.add(async () => {
        let dependencies = event.bundleGraph.getDependencies(asset);
        let depsByBundle = {};
        for (let bundle of event.bundleGraph.getBundlesWithAsset(asset)) {
          let deps = {};
          for (let dep of dependencies) {
            let resolved = event.bundleGraph.getResolvedAsset(dep, bundle);
            if (resolved) {
              deps[getSpecifier(dep)] =
                event.bundleGraph.getAssetPublicId(resolved);
            }
          }
          depsByBundle[bundle.id] = deps;
        }

        return {
          id: event.bundleGraph.getAssetPublicId(asset),
          url: this.getSourceURL(asset),
          type: asset.type,
          // No need to send the contents of non-JS assets to the client.
          output:
            asset.type === 'js' ? await this.getHotAssetContents(asset) : '',
          envHash: asset.env.id,
          depsByBundle,
        };
      });
    }

    let assets = await queue.run();
    this.broadcast({
      type: 'update',
      assets: assets,
    });
  }

  getHotAssetContents(asset: Asset) {
    let output = await asset.getCode();
    if (asset.type === 'js') {
      let publicId = this.bundleGraph.getAssetPublicId(asset);
      output = `parcelHotUpdate['${publicId}'] = function (require, module, exports) {${output}}`;
    }

    let sourcemap = await asset.getMap();
    if (sourcemap) {
      let sourcemapStringified = await sourcemap.stringify({
        format: 'inline',
        sourceRoot: SOURCES_ENDPOINT + '/',
        // $FlowFixMe
        fs: asset.fs,
      });

      invariant(typeof sourcemapStringified === 'string');
      output += `\n//# sourceMappingURL=${sourcemapStringified}`;
      output += `\n//# sourceURL=${encodeURI(this.getSourceURL(asset))}\n`;
    }

    return output;
  }

  getSourceURL(asset: Asset) {
    let origin = '';
    if (!this.options.devServer) {
      origin = `http://${this.options.host || 'localhost'}:${
        this.options.port
      }`;
    }
    return origin + HMR_ENDPOINT + '/' + asset.id;
  }

  handleSocketError(err: ServerError) {
    if (err.code === 'ECONNRESET') {
      // This gets triggered on page refresh, ignore this
      return;
    }

    this.options.logger.warn({
      origin: '@parcel/reporter-dev-server',
      message: `[${err.code}]: ${err.message}`,
      stack: err.stack,
    });
  }

  broadcast(msg: HMRMessage) {
    const json = JSON.stringify(msg);
    for (let ws of this.wss.clients) {
      ws.send(json);
    }
  }
}

function getSpecifier(dep: Dependency): string {
  if (typeof dep.meta.placeholder === 'string') {
    return dep.meta.placeholder;
  }

  return dep.specifier;
}

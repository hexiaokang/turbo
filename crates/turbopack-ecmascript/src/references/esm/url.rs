use anyhow::Result;
use swc_core::{
    ecma::ast::{Expr, ExprOrSpread, NewExpr},
    quote,
};
use turbo_tasks::{primitives::StringVc, Value, ValueToString, ValueToStringVc};
use turbopack_core::{
    chunk::{
        ChunkableAssetReference, ChunkableAssetReferenceVc, ChunkingContextVc, ChunkingType,
        ChunkingTypeOptionVc,
    },
    environment::{Rendering, RenderingVc},
    issue::{code_gen::CodeGenerationIssue, IssueSeverity},
    reference::{AssetReference, AssetReferenceVc},
    reference_type::UrlReferenceSubType,
    resolve::{
        origin::{ResolveOrigin, ResolveOriginVc},
        parse::RequestVc,
        ResolveResultVc,
    },
};

use super::base::{ReferencedAsset, ReferencedAssetVc};
use crate::{
    chunk::{EcmascriptChunkItem, EcmascriptChunkPlaceable},
    code_gen::{CodeGenerateable, CodeGenerateableVc, CodeGeneration, CodeGenerationVc},
    create_visitor,
    references::AstPathVc,
    resolve::url_resolve,
    utils::module_id_to_lit,
};

/// URL Asset References are injected during code analysis when we find a
/// (staticly analyzable) `new URL("path", import.meta.url)`.
///
/// It's responsible rewriting the `URL` constructor's arguments to allow the
/// referenced file to be imported/fetched/etc.
#[turbo_tasks::value]
pub struct UrlAssetReference {
    origin: ResolveOriginVc,
    request: RequestVc,
    rendering: RenderingVc,
    ast_path: AstPathVc,
}

#[turbo_tasks::value_impl]
impl UrlAssetReferenceVc {
    #[turbo_tasks::function]
    pub fn new(
        origin: ResolveOriginVc,
        request: RequestVc,
        rendering: RenderingVc,
        ast_path: AstPathVc,
    ) -> Self {
        UrlAssetReference {
            origin,
            request,
            rendering,
            ast_path,
        }
        .cell()
    }

    #[turbo_tasks::function]
    pub(super) async fn get_referenced_asset(self) -> Result<ReferencedAssetVc> {
        let this = self.await?;
        Ok(ReferencedAssetVc::from_resolve_result(
            url_resolve(
                this.origin,
                this.request,
                Value::new(UrlReferenceSubType::EcmaScriptNewUrl),
            ),
            this.request,
        ))
    }
}

#[turbo_tasks::value_impl]
impl AssetReference for UrlAssetReference {
    #[turbo_tasks::function]
    async fn resolve_reference(&self) -> ResolveResultVc {
        url_resolve(
            self.origin,
            self.request,
            Value::new(UrlReferenceSubType::EcmaScriptNewUrl),
        )
    }
}

#[turbo_tasks::value_impl]
impl ValueToString for UrlAssetReference {
    #[turbo_tasks::function]
    async fn to_string(&self) -> Result<StringVc> {
        Ok(StringVc::cell(format!(
            "new URL({})",
            self.request.to_string().await?,
        )))
    }
}

#[turbo_tasks::value_impl]
impl ChunkableAssetReference for UrlAssetReference {
    #[turbo_tasks::function]
    fn chunking_type(&self, _context: ChunkingContextVc) -> ChunkingTypeOptionVc {
        ChunkingTypeOptionVc::cell(Some(ChunkingType::PlacedOrParallel))
    }
}

#[turbo_tasks::value_impl]
impl CodeGenerateable for UrlAssetReference {
    #[turbo_tasks::function]
    async fn code_generation(
        self_vc: UrlAssetReferenceVc,
        context: ChunkingContextVc,
    ) -> Result<CodeGenerationVc> {
        let this = self_vc.await?;
        let mut visitors = vec![];

        let referenced_asset = self_vc.get_referenced_asset().await?;

        // For rendering environments (CSR and SSR), we rewrite the `import.meta.url` to
        // be a location.origin because it allows us to access files from the root of
        // the dev server. It's important that this be rewritten for SSR as well, so
        // that the client's hydration matches exactly.
        //
        // In a non-rendering env, the `import.meta.url` is already the correct `file://` URL
        // to load files.
        let rewrite = match &*this.rendering.await? {
            Rendering::None => {
                CodeGenerationIssue {
                    severity: IssueSeverity::Error.into(),
                    title: StringVc::cell(
                        "new URL(…) not implemented for this environment".to_string(),
                    ),
                    message: StringVc::cell(
                        "new URL(…) is only currently supported for rendering environments like \
                         Client-Side or Server-Side Rendering."
                            .to_string(),
                    ),
                    path: this.origin.origin_path(),
                }
                .cell()
                .as_issue()
                .emit();
                None
            }
            Rendering::Client => Some(quote!("location.origin" as Expr)),
            Rendering::Server(server_addr) => {
                let location = server_addr.await?.to_string()?;
                Some(location.into())
            }
        };

        let ast_path = this.ast_path.await?;

        match &*referenced_asset {
            ReferencedAsset::Some(asset) => {
                // We rewrite the first `new URL()` arguments to be a require() of the chunk
                // item, which exports the static asset path to the linked file.
                let id = asset.as_chunk_item(context).id().await?;

                visitors.push(
                    create_visitor!(ast_path, visit_mut_expr(new_expr: &mut Expr) {
                        if let Expr::New(NewExpr { args: Some(args), .. }) = new_expr {
                            if let Some(ExprOrSpread { box expr, spread: None }) = args.get_mut(0) {
                                *expr = quote!(
                                    "__turbopack_require__($id)" as Expr,
                                    id: Expr = module_id_to_lit(&id),
                                );
                            }

                            if let Some(rewrite) = &rewrite {
                                if let Some(ExprOrSpread { box expr, spread: None }) = args.get_mut(1) {
                                    *expr = rewrite.clone();
                                }
                            }
                        }
                    }),
                );
            }
            ReferencedAsset::OriginalReferenceTypeExternal(request) => {
                let request = request.to_string();
                visitors.push(
                    create_visitor!(ast_path, visit_mut_expr(new_expr: &mut Expr) {
                        if let Expr::New(NewExpr { args: Some(args), .. }) = new_expr {
                            if let Some(ExprOrSpread { box expr, spread: None }) = args.get_mut(0) {
                                *expr = request.as_str().into()
                            }

                            if let Some(rewrite) = &rewrite {
                                if let Some(ExprOrSpread { box expr, spread: None }) = args.get_mut(1) {
                                    *expr = rewrite.clone();
                                }
                            }
                        }
                    }),
                );
            }
            ReferencedAsset::None => {}
        }

        Ok(CodeGeneration { visitors }.into())
    }
}

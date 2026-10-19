//! 宿主装配：把模型、输入、工具、插件与审批口装成一个可复用的运行器。
//!
//! 装配在 [`HostBuilder::build`] 里一次性解析 —— 缺模型、工具重名这类
//! 自包含的配置错误在装载期大声失败，而不是等到第一次调用。`Host` 拥有
//! 工具注册表与扩展内核，因此插件状态可以跨 turn 保留，工具也可以在
//! 运行之间增删。

use std::fmt;
use std::sync::Arc;

use agnes_core::config::ModelName;
use agnes_core::llm::RequestBuilder;
use agnes_core::turn::{AgentConfig, Turn};
use agnes_io::input::Input as InputPort;
use agnes_io::llm::Llm;
use agnes_io::session::SessionLog;
use agnes_io::tool::{DuplicateTool, Tool, ToolRegistry};
use jiff::Timestamp;

use crate::approval::ApprovalChannel;
use crate::driver::{Driver, Outlets, Ports};
use crate::ext::{Bundle, Extensions, Plugin};
use crate::{AgentError, Event, RunSummary, Sink, StandardRequestBuilder};

/// 装载期的失败：自包含的配置错误在这里暴露。
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("no model configured: set `model` or supply a request builder")]
    MissingModel,
    /// 一条永远不可能命中的规则（工具名拼错、前缀无人匹配）。
    #[error("bundle `{bundle}` has a rule that can never fire: {rule}")]
    UnknownTool { bundle: &'static str, rule: String },
    /// 配了 ask 规则，却没有接审批口 —— 运行期只会全部失败。
    #[error("bundle `{bundle}` asks for approval, but no approval channel is wired")]
    AskWithoutApprovals { bundle: &'static str },
    #[error(transparent)]
    DuplicateTool(#[from] DuplicateTool),
}

/// 一次装配好的运行器。
pub struct Host<'a> {
    llm: &'a dyn Llm,
    input: &'a dyn InputPort,
    requests: Box<dyn RequestBuilder + 'a>,
    approvals: Option<&'a ApprovalChannel>,
    tools: ToolRegistry,
    extensions: Extensions,
}

impl fmt::Debug for Host<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host")
            .field(
                "tools",
                &self
                    .tools
                    .names()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("plugins", &self.extensions.names().collect::<Vec<_>>())
            .field("approvals", &self.approvals.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> Host<'a> {
    pub fn builder(llm: &'a dyn Llm, input: &'a dyn InputPort) -> HostBuilder<'a> {
        HostBuilder::new(llm, input)
    }

    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub fn tools_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tools
    }

    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }

    /// 是否接了审批口 —— 也是「ask 能否真的问出去」的判据。
    pub fn has_approvals(&self) -> bool {
        self.approvals.is_some()
    }

    /// 一次完整 run：从 [`Turn::start`] 到收场。
    ///
    /// 控制：运行中随时可打断（[`InputPort::interrupted`]）—— 模型响应、
    /// 工具执行、审批挂起三个等待点都适用，in-flight 的 future 就地丢弃；
    /// 运行中的插话（[`InputPort::drain`]）由机器缓存、在下一个 step 边界
    /// 生效，没生效的随 [`RunSummary::pending_user_messages`] 还给宿主。
    ///
    /// 工具调用先过扩展内核的门禁折叠 —— 拒绝以合成失败结果推进机器、
    /// `Ask` 则走审批口问询挂起（未接线即拒绝）；结果入账前再过一次
    /// 结果折叠。工具失败不中止 run —— 它是给模型的错误输出；只有传输/
    /// 模型错误终止。
    pub async fn run(
        &mut self,
        config: AgentConfig,
        sink: &mut dyn Sink,
        session: Option<&mut dyn SessionLog>,
    ) -> Result<RunSummary, AgentError> {
        let started_at = Timestamp::now();
        let goal = config.goal.clone();
        let tools = config
            .context
            .environment()
            .tools()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        sink.event(&Event::Started {
            at: started_at,
            goal,
            tools,
        });

        let ports = Ports {
            registry: &self.tools,
            llm: self.llm,
            input: self.input,
            requests: self.requests.as_ref(),
            extensions: &mut self.extensions,
            approvals: self.approvals,
        };
        let outlets = Outlets {
            sink: &mut *sink,
            session,
        };
        let turn = Turn::start(config);
        // 驱动状态机一路跑到收场（收场前的报告事件由它的泵补完）。
        let mut closed = Driver::new(turn, ports, outlets).run().await?;

        let finished_at = Timestamp::now();
        let outcome = closed.outcome().clone();
        let steps_used = closed.steps_used();
        let usage = closed.usage();
        let pending_user_messages = closed.take_unapplied_user_messages();
        let context = closed.into_context();
        let summary = RunSummary {
            outcome: outcome.clone(),
            steps_used,
            usage,
            context,
            pending_user_messages,
            started_at,
            finished_at,
        };
        sink.event(&Event::Finished {
            at: finished_at,
            outcome,
        });
        Ok(summary)
    }
}

/// 装配器：缺省解析与装载校验都集中在 [`HostBuilder::build`]。
pub struct HostBuilder<'a> {
    llm: &'a dyn Llm,
    input: &'a dyn InputPort,
    model: Option<ModelName>,
    requests: Option<Box<dyn RequestBuilder + 'a>>,
    approvals: Option<&'a ApprovalChannel>,
    tools: Vec<Arc<dyn Tool>>,
    plugins: Vec<Box<dyn Plugin>>,
    bundles: Vec<Box<dyn Bundle>>,
}

impl<'a> HostBuilder<'a> {
    fn new(llm: &'a dyn Llm, input: &'a dyn InputPort) -> Self {
        Self {
            llm,
            input,
            model: None,
            requests: None,
            approvals: None,
            tools: Vec::new(),
            plugins: Vec::new(),
            bundles: Vec::new(),
        }
    }

    /// 模型名：未提供请求转换者时，用它构造标准投影。
    pub fn model(mut self, model: ModelName) -> Self {
        self.model = Some(model);
        self
    }

    /// 替换请求转换者（提示词怎么来由宿主决定）。
    pub fn request_builder(mut self, requests: Box<dyn RequestBuilder + 'a>) -> Self {
        self.requests = Some(requests);
        self
    }

    /// 接审批口；不接则任何 `Ask` 都按拒绝落定（fail-closed）。
    pub fn approvals(mut self, approvals: Option<&'a ApprovalChannel>) -> Self {
        self.approvals = approvals;
        self
    }

    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// 一次装入一组工具（顺序无关；注册表按名字排序）。
    pub fn tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
        self.plugins.push(Box::new(plugin));
        self
    }

    /// 追加一个装载单元；在 [`HostBuilder::build`] 里按注册顺序应用。
    pub fn bundle(mut self, bundle: impl Bundle + 'static) -> Self {
        self.bundles.push(Box::new(bundle));
        self
    }

    pub fn build(self) -> Result<Host<'a>, LoadError> {
        let requests = match (self.requests, self.model) {
            (Some(requests), _) => requests,
            (None, Some(model)) => Box::new(StandardRequestBuilder::new(model)),
            (None, None) => return Err(LoadError::MissingModel),
        };
        let tools = ToolRegistry::build(self.tools)?;
        let mut extensions = Extensions::new();
        for plugin in self.plugins {
            extensions.register(plugin);
        }
        let mut host = Host {
            llm: self.llm,
            input: self.input,
            requests,
            approvals: self.approvals,
            tools,
            extensions,
        };
        // 装载单元按注册顺序应用：前者装上的工具对后者的规则校验可见。
        for bundle in self.bundles {
            bundle.install(&mut host)?;
        }
        Ok(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{PolicyBundle, Rule};
    use crate::tools::{FsBundle, ReadFile};
    use agnes_core::llm::{LlmRequest, LlmResponse};
    use agnes_core::tool::ToolName;
    use agnes_io::input::NoInput;
    use agnes_io::llm::LlmError;
    use async_trait::async_trait;
    use std::str::FromStr;

    /// 不会真正被调用的模型。
    struct Never;

    #[async_trait]
    impl Llm for Never {
        async fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
            std::future::pending().await
        }
    }

    fn model() -> ModelName {
        ModelName::from_str("test-model").unwrap()
    }

    fn name(tool: &str) -> ToolName {
        ToolName::from_str(tool).unwrap()
    }

    #[test]
    fn a_missing_model_fails_at_load() {
        let llm = Never;
        let input = NoInput;
        let err = Host::builder(&llm, &input).build().unwrap_err();
        assert!(matches!(err, LoadError::MissingModel));
    }

    #[test]
    fn a_rule_that_can_never_fire_fails_at_load() {
        let llm = Never;
        let input = NoInput;

        // 精确规则指向不存在的工具
        let err = Host::builder(&llm, &input)
            .model(model())
            .bundle(PolicyBundle::new(vec![Rule::Name(name("nope"))], vec![]))
            .build()
            .unwrap_err();
        assert!(matches!(err, LoadError::UnknownTool { .. }));

        // 前缀规则无人匹配
        let err = Host::builder(&llm, &input)
            .model(model())
            .bundle(FsBundle::new(".", false))
            .bundle(PolicyBundle::new(vec![Rule::Prefix("zzz".into())], vec![]))
            .build()
            .unwrap_err();
        assert!(matches!(err, LoadError::UnknownTool { .. }));
    }

    #[test]
    fn asking_without_an_approval_channel_fails_at_load() {
        let llm = Never;
        let input = NoInput;
        let err = Host::builder(&llm, &input)
            .model(model())
            .bundle(FsBundle::new(".", false))
            .bundle(PolicyBundle::new(
                vec![],
                vec![Rule::Name(name("read_file"))],
            ))
            .build()
            .unwrap_err();
        assert!(matches!(err, LoadError::AskWithoutApprovals { .. }));
    }

    #[test]
    fn a_valid_policy_installs_the_plugin() {
        let (approval, _questions) = ApprovalChannel::channel(None);
        let llm = Never;
        let input = NoInput;
        let host = Host::builder(&llm, &input)
            .model(model())
            .bundle(FsBundle::new(".", false))
            .bundle(PolicyBundle::new(
                vec![],
                vec![Rule::Name(name("read_file"))],
            ))
            .approvals(Some(&approval))
            .build()
            .unwrap();
        assert_eq!(
            host.extensions().names().collect::<Vec<_>>(),
            ["permission"]
        );
    }

    #[test]
    fn a_bundle_registers_tools_and_duplicates_fail() {
        let dir = tempfile::tempdir().unwrap();
        let llm = Never;
        let input = NoInput;
        let mut host = Host::builder(&llm, &input)
            .model(model())
            .bundle(FsBundle::new(dir.path(), true))
            .build()
            .unwrap();
        assert!(host.tools().contains(&name("read_file")));
        assert!(host.tools().contains(&name("run_shell")));
        assert!(
            host.tools_mut()
                .register(Arc::new(ReadFile::new(dir.path())))
                .is_err()
        );
    }
}

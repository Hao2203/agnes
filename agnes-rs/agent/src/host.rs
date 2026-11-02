//! 宿主装配：把模型、输入、工具、插件与审批口装成一个可复用的运行器。
//!
//! 装配在 [`HostBuilder::build`] 里一次性解析 —— 缺模型、工具重名这类
//! 自包含的配置错误在装载期大声失败，而不是等到第一次调用。`Host` 拥有
//! 工具注册表与扩展内核，因此插件状态可以跨 turn 保留，工具也可以在
//! 运行之间增删。

use std::fmt;
use std::sync::Arc;

use agnes_core::agent::Agent;
use agnes_core::config::ModelName;
use agnes_core::turn::{AgentConfig, Turn};
use agnes_io::input::Input as InputPort;
use agnes_io::llm::Llm;
use agnes_io::tool::{DuplicateTool, Tool, ToolRegistry};
use jiff::Timestamp;

use crate::approval::ApprovalChannel;
use crate::driver::{Driver, Outlets, Ports, Start};
use crate::ext::{Bundle, Extensions, Origin, Plugin};
use crate::{
    AgentError, Event, Log, RequestBuilder, RunSummary, SessionLog, Sink, StandardRequestBuilder,
};

/// 装载期的失败：自包含的配置错误在这里暴露。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    #[error("no model configured: set `model` or supply a request builder")]
    MissingModel,
    /// 一条永远不可能命中的规则（工具名拼错、前缀无人匹配）。
    #[error("bundle `{bundle}` has a rule that can never fire: {rule}")]
    UnknownTool { bundle: Origin, rule: String },
    /// 配了 ask 规则，却没有接审批口 —— 运行期只会全部失败。
    #[error("bundle `{bundle}` asks for approval, but no approval channel is wired")]
    AskWithoutApprovals { bundle: Origin },
    #[error(transparent)]
    DuplicateTool(#[from] DuplicateTool),
}

/// 一次 run 的全部输入：配置、已有日志、上报出口与可选会话。
#[non_exhaustive]
pub struct RunRequest<'s, 'q> {
    pub config: AgentConfig,
    pub log: Log,
    pub sink: &'s mut dyn Sink,
    pub session: Option<&'q mut dyn SessionLog>,
}

impl<'s, 'q> RunRequest<'s, 'q> {
    pub fn new(config: AgentConfig, log: Log, sink: &'s mut dyn Sink) -> Self {
        Self {
            config,
            log,
            sink,
            session: None,
        }
    }

    pub fn with_session(mut self, session: &'q mut dyn SessionLog) -> Self {
        self.session = Some(session);
        self
    }
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

    /// 一次完整 run：从就绪阶段（有/无 goal）到收场。
    ///
    /// 控制：运行中随时可打断（[`InputPort::interrupted`]）—— 模型响应、
    /// 工具执行、审批挂起三个等待点都适用，in-flight 的 future 就地丢弃；
    /// 运行中的注入（[`InputPort::drain`] 的用户消息）由机器缓存、在下一个
    /// step 边界生效，没生效的随 [`RunSummary::pending_messages`] 还给宿主。
    ///
    /// 工具调用先发布 `ToolCallRequested`，再取用插件指令 —— 拒绝以合成
    /// 失败结果推进机器、`RequireApproval` 走审批口问询挂起（未接线即
    /// 拒绝）；结果入账前发布 `ToolCallFinished` 并取用替换。注入进队列、
    /// 工具变更在下一个模型调用前落地、`Abort` 在下一个工作前收场。
    /// 工具失败不中止 run —— 它是给模型的错误输出；只有传输/模型错误终止。
    ///
    /// # Errors
    ///
    /// 模型/传输失败会中止 run；会话写盘失败按 fail-closed 中止；内部状态
    /// 不变式被破坏时返回 [`AgentError::Invariant`]。
    pub async fn run(&mut self, request: RunRequest<'_, '_>) -> Result<RunSummary, AgentError> {
        let RunRequest {
            config,
            log,
            sink,
            session,
        } = request;
        let started_at = Timestamp::now();
        let AgentConfig {
            header,
            goal,
            budget,
        } = config;
        let tools = header
            .environment()
            .tools()
            .map(|spec| spec.name().clone())
            .collect();
        sink.event(&Event::Started {
            at: started_at,
            goal: goal.clone(),
            tools,
        });

        // 上一次失败 run 遗留的指令/注入不能污染这一次。
        self.extensions.clear_pending_commands();
        self.extensions.clear_pending_injections();

        let ports = Ports {
            registry: &mut self.tools,
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
        // turn 从持久化日志的历史消息重建；系统提示与工具环境以当前
        // 配置为准。goal 可选：有则先留在就绪阶段，无则直接基于历史启动。
        let turn = Turn::with_messages(header, budget, log.messages().cloned().collect::<Vec<_>>());
        let start = match goal {
            Some(goal) => Start::Goal(Agent::with_goal(turn, goal)),
            None => Start::Empty(Agent::new(turn)),
        };
        let mut log = log;
        // 驱动状态机一路跑到收场（收场前的报告事件由它的泵补完）；日志
        // 由驱动记录，宿主保留所有权。
        let (closed, aborted) = match Driver::new(&mut log, ports, outlets).run(start).await {
            Ok(done) => done,
            Err(err) => {
                // 失败 run 里已经产生的工具变更对下一 turn 仍然有效；
                // 无法执行的定向指令与注入直接丢弃。
                for change in self.extensions.take_tool_changes() {
                    change.apply(&mut self.tools);
                }
                self.extensions.clear_pending_commands();
                self.extensions.clear_pending_injections();
                sink.event(&Event::Failed {
                    at: Timestamp::now(),
                    error: err.to_string(),
                });
                return Err(err);
            }
        };

        let finished_at = Timestamp::now();
        let result = closed.into_result();
        // 收场时仍在扩展 outbox 里的消息交给宿主；用户输入留在 Input 端口。
        let pending_messages = self
            .extensions
            .take_pending_injections()
            .into_iter()
            .map(|queued| queued.into_message())
            .collect();
        let summary = RunSummary {
            outcome: result.outcome,
            steps_used: result.steps_used,
            usage: result.usage,
            log,
            pending_messages,
            aborted,
            started_at,
            finished_at,
        };
        sink.event(&Event::Finished {
            at: finished_at,
            outcome: result.outcome,
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

    /// # Errors
    ///
    /// Returns [`LoadError`] when the model is missing, a tool name is
    /// duplicated, or a bundle/config check fails.
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
    use crate::permission::{PermissionRules, PolicyBundle, Rule};
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
            .bundle(PolicyBundle::new(PermissionRules {
                deny: vec![Rule::Name(name("nope"))],
                ask: vec![],
            }))
            .build()
            .unwrap_err();
        assert!(matches!(err, LoadError::UnknownTool { .. }));

        // 前缀规则无人匹配
        let err = Host::builder(&llm, &input)
            .model(model())
            .bundle(FsBundle::new(".", false))
            .bundle(PolicyBundle::new(PermissionRules {
                deny: vec![Rule::Prefix("zzz".into())],
                ask: vec![],
            }))
            .build()
            .unwrap_err();
        assert!(matches!(err, LoadError::UnknownTool { .. }));
    }

    #[test]
    fn unresolved_rules_can_be_allowed_for_dynamic_tools() {
        let llm = Never;
        let input = NoInput;
        let host = Host::builder(&llm, &input)
            .model(model())
            .bundle(
                PolicyBundle::new(PermissionRules {
                    deny: vec![Rule::Name(name("later"))],
                    ask: vec![],
                })
                .allow_unresolved(),
            )
            .build()
            .unwrap();
        assert!(
            host.extensions()
                .names()
                .any(|plugin| plugin == "permission")
        );
    }

    #[test]
    fn asking_without_an_approval_channel_fails_at_load() {
        let llm = Never;
        let input = NoInput;
        let err = Host::builder(&llm, &input)
            .model(model())
            .bundle(FsBundle::new(".", false))
            .bundle(PolicyBundle::new(PermissionRules {
                deny: vec![],
                ask: vec![Rule::Name(name("read_file"))],
            }))
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
            .bundle(PolicyBundle::new(PermissionRules {
                deny: vec![],
                ask: vec![Rule::Name(name("read_file"))],
            }))
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

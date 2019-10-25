use glib::futures::{
    channel::mpsc::{unbounded, UnboundedSender},
    stream::{select, Stream},
    task::Context,
    Future, Poll, StreamExt,
};
use glib::{ObjectExt, WeakRef};
use gtk::{Container, Widget};

use std::any::TypeId;
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::RwLock;

use crate::scope::{AnyScope, Scope};
use crate::vdom::State;
use crate::vnode::VNode;

pub trait Component: Default + Unpin {
    type Message: Clone + Send + Debug;
    type Properties: Clone + Default;
    fn update(&mut self, _msg: Self::Message) -> bool {
        false
    }

    fn create(_props: Self::Properties) -> Self {
        Self::default()
    }

    fn change(&mut self, _props: Self::Properties) -> bool {
        unimplemented!()
    }

    fn mounted(&mut self) {}

    fn unmounted(&mut self) {}

    fn view(&self) -> VNode<Self>;
}

impl Component for () {
    type Message = ();
    type Properties = ();
    fn view(&self) -> VNode<Self> {
        unimplemented!("tried to render a null component")
    }
}

pub(crate) enum ComponentMessage<C: Component> {
    Update(C::Message),
    Props(C::Properties),
    Mounted,
    Unmounted,
}

impl<C: Component> Clone for ComponentMessage<C> {
    fn clone(&self) -> Self {
        match self {
            ComponentMessage::Update(msg) => ComponentMessage::Update(msg.clone()),
            ComponentMessage::Props(props) => ComponentMessage::Props(props.clone()),
            ComponentMessage::Mounted => ComponentMessage::Mounted,
            ComponentMessage::Unmounted => ComponentMessage::Unmounted,
        }
    }
}

pub struct ComponentTask<C, P>
where
    C: Component,
    P: Component,
{
    scope: Scope<C>,
    parent_scope: Option<Scope<P>>,
    state: C,
    ui_state: State<C>,
    channel: Pin<Box<dyn Stream<Item = ComponentMessage<C>>>>,
}

impl<C, P> ComponentTask<C, P>
where
    C: 'static + Component,
    P: 'static + Component,
{
    pub(crate) fn new(
        props: C::Properties,
        parent: Option<&Container>,
        parent_scope: Option<&Scope<P>>,
    ) -> (Scope<C>, UnboundedSender<ComponentMessage<C>>, Self) {
        let (sys_send, sys_recv) = unbounded();
        let (user_send, user_recv) = unbounded();

        // As `C::Message` must be `Send` but `C::Properties` can't be,
        // we keep two senders but merge them into a single receiver at
        // the task end.
        let channel = Pin::new(Box::new(select(
            user_recv.map(ComponentMessage::Update),
            sys_recv,
        )));

        let type_name = std::any::type_name::<C>();
        let scope = match parent_scope {
            Some(ref p) => p.inherit(type_name, user_send),
            None => Scope::new(type_name, user_send),
        };
        let state = C::create(props);
        let initial_view = state.view();
        let ui_state = State::build(&initial_view, parent, &scope);
        (
            scope.clone(),
            sys_send,
            ComponentTask {
                scope,
                parent_scope: parent_scope.cloned(),
                state,
                ui_state,
                channel,
            },
        )
    }

    pub fn process(&mut self, ctx: &mut Context) -> Poll<()> {
        let mut render = false;
        loop {
            match Stream::poll_next(self.channel.as_mut(), ctx) {
                Poll::Ready(Some(msg)) => match msg {
                    ComponentMessage::Update(msg) => {
                        if self.state.update(msg) {
                            render = true;
                        }
                    }
                    ComponentMessage::Props(props) => {
                        if self.state.change(props) {
                            render = true;
                        }
                    }
                    ComponentMessage::Mounted => {
                        self.state.mounted();
                    }
                    ComponentMessage::Unmounted => {
                        self.state.unmounted();
                    }
                },
                Poll::Pending if render => {
                    // we patch
                    let new_view = self.state.view();
                    self.scope.mute();
                    if !self.ui_state.patch(&new_view, None, &self.scope) {
                        unimplemented!("don't know how to propagate failed patch");
                    }
                    self.scope.unmute();
                    return Poll::Pending;
                }
                Poll::Ready(None) => {
                    println!("task terminating because channel handles all dropped");
                    return Poll::Ready(());
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    pub fn widget(&self) -> Widget {
        self.ui_state.object().clone()
    }

    pub(crate) fn current_parent_scope() -> Scope<C> {
        LOCAL_CONTEXT.with(|key| {
            let lock = key.read().unwrap();
            match &lock.parent_scope {
                None => panic!("current task has no parent scope set!"),
                Some(any_scope) => match any_scope.try_get::<C>() {
                    None => panic!(
                        "unexpected type for current parent scope (expected {:?})",
                        TypeId::of::<C::Properties>()
                    ),
                    Some(scope) => scope.clone(),
                },
            }
        })
    }
}

pub fn current_widget() -> Option<Widget> {
    LOCAL_CONTEXT.with(|key| {
        let lock = key.read().unwrap();
        lock.current_widget
            .as_ref()
            .and_then(|widget| widget.upgrade())
    })
}

#[derive(Default)]
struct LocalContext {
    parent_scope: Option<AnyScope>,
    current_widget: Option<WeakRef<Widget>>,
}

thread_local! {
    static LOCAL_CONTEXT: RwLock<LocalContext> = RwLock::new(Default::default())
}

impl<C, P> Future for ComponentTask<C, P>
where
    C: 'static + Component,
    P: 'static + Component,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        LOCAL_CONTEXT.with(|key| {
            *key.write().unwrap() = LocalContext {
                parent_scope: self.parent_scope.as_ref().map(|scope| scope.clone().into()),
                current_widget: Some(self.ui_state.object().downgrade()),
            };
        });
        let polled = self.get_mut().process(ctx);
        LOCAL_CONTEXT.with(|key| {
            *key.write().unwrap() = Default::default();
        });
        polled
    }
}

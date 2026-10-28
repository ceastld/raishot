//! 显式点击到新绘制回调之间的一次性意图。
//! 宿主负责源、选区和焦点校验；此处只保证同一请求最多消费一次。

use std::sync::Arc;

#[derive(Clone)]
pub struct PaintTicket {
    intent: Arc<()>,
    paint: Arc<()>,
}

struct Pending<T> {
    intent: Arc<()>,
    payload: T,
    paint: Option<Arc<()>>,
}

pub struct ClickIntent<T> {
    pending: Option<Pending<T>>,
}

impl<T> Default for ClickIntent<T> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<T> ClickIntent<T> {
    /// 只由宿主的真实点击入口排队；替换时立即释放旧请求资源。
    pub fn queue(&mut self, token: Arc<()>, payload: T) {
        self.pending = Some(Pending {
            intent: token,
            payload,
            paint: None,
        });
    }

    pub fn payload(&self) -> Option<&T> {
        self.pending.as_ref().map(|pending| &pending.payload)
    }

    pub fn intent_token(&self) -> Option<&Arc<()>> {
        self.pending.as_ref().map(|pending| &pending.intent)
    }

    /// 宿主在新的实际绘制中调用；重复绘制不会重复安排回调。
    pub fn painted(&mut self, current: bool) -> Option<PaintTicket> {
        if !current {
            self.cancel();
            return None;
        }
        let pending = self.pending.as_mut()?;
        if pending.paint.is_some() {
            return None;
        }
        let paint = Arc::new(());
        pending.paint = Some(paint.clone());
        Some(PaintTicket {
            intent: pending.intent.clone(),
            paint,
        })
    }

    /// 匹配意图和绘制身份后先消费，再按宿主的当前校验交付。
    /// 旧回调无权删除后来替换的请求。
    pub fn take(&mut self, ticket: &PaintTicket, current: bool) -> Option<T> {
        let pending = self.pending.as_ref()?;
        let paint = pending.paint.as_ref()?;
        if !Arc::ptr_eq(&pending.intent, &ticket.intent) || !Arc::ptr_eq(paint, &ticket.paint) {
            return None;
        }
        let pending = self.pending.take()?;
        current.then_some(pending.payload)
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// 订阅回调只可取消自己登记的点击意图。
    pub fn cancel_token(&mut self, token: &Arc<()>) -> bool {
        let matches = self
            .pending
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(&pending.intent, token));
        if matches {
            self.cancel();
        }
        matches
    }
}
